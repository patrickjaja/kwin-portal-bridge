use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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
use crate::error_kind::{ErrorKind, kind_of, kinded};
use crate::executor::ExecutorBackend;
use crate::kwin::KWinBackend;
use crate::model::{
    ClipboardReadResult, ClipboardWriteResult, PortalSessionInfo, ScreenInfo, ScreenshotCapture,
    ScreenshotResult,
};
use crate::portal::{ClosedSignal, LivePortalSession};
use crate::session_overlay::SessionOverlayProcess;
use crate::teach_overlay::TeachOverlayProcess;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

/// Upper bound for reading a request from / writing a response to one IPC
/// client. Connections are serviced sequentially, so without this a single
/// stalled client freezes the daemon (including SIGTERM handling).
const IPC_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a liveness probe may wait for the daemon's reply. The daemon
/// binds its socket BEFORE the portal session is up, so while the consent
/// dialog is open a connect succeeds but no reply comes; an unbounded probe
/// would hang `session-start` / `session-end` behind the dialog.
const PING_TIMEOUT: Duration = Duration::from_secs(2);
/// How long `session-start` waits for the daemon to become ready. Covers a
/// human reading and answering the consent dialog, with margin; the JS
/// executor's session-start budget (125 s) sits above this.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Poll interval for the daemon's liveness while waiting for it.
const READY_POLL: Duration = Duration::from_millis(150);
/// How long `session-end` waits for the Shutdown reply. A daemon that does
/// not answer within this (stopped, wedged, behind its consent dialog) goes
/// straight to SIGTERM.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);
/// Grace period after a Shutdown reply for the daemon to tear down and exit.
/// Above `TEARDOWN_LIMIT`, so a daemon that answered is never killed while
/// still inside its own (bounded) teardown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Grace period after SIGTERM before SIGKILL.
/// Grace period after SIGTERM before SIGKILL. Above `TEARDOWN_LIMIT`, so a
/// daemon that handles SIGTERM finishes its own teardown (window restore,
/// portal close) before the group SIGKILL. Silent-daemon total: 1.5 s
/// Shutdown request + 2.7 s + kill, inside the JS 5 s exit budget.
const TERM_GRACE: Duration = Duration::from_millis(2700);
/// The daemon's own teardown budget (window restore, overlays, portal
/// session). Past it the daemon hard-exits.
const TEARDOWN_LIMIT: Duration = Duration::from_millis(2500);
/// Budget for closing the portal session inside the teardown.
const PORTAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1500);
/// Budget for one clipboard read/write inside the daemon. wl-clipboard reads
/// until the source closes its pipe, so a misbehaving clipboard owner would
/// otherwise wedge the (sequential) daemon for good.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
/// Client-side budget for one daemon request (see `request_timeout`).
const REQUEST_BASE_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a failed request waits for a Closed signal that explains it. The
/// portal sends Closed before it starts refusing calls on the session, so it
/// is normally queued already; a PipeWire stream can end a little earlier.
const CLOSED_GRACE: Duration = Duration::from_millis(500);

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

/// A CLOCK_MONOTONIC instant as a raw timespec. Client and daemon run on the
/// same machine, so the daemon can compare a client's deadline to its own
/// clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MonoTime {
    sec: i64,
    nsec: i64,
}

impl MonoTime {
    pub fn now() -> Self {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes into the valid timespec we pass.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        Self {
            sec: ts.tv_sec,
            nsec: ts.tv_nsec,
        }
    }

    fn after(self, duration: Duration) -> Self {
        let nsec = self.nsec + i64::from(duration.subsec_nanos());
        Self {
            sec: self
                .sec
                .saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
                .saturating_add(nsec / 1_000_000_000),
            nsec: nsec % 1_000_000_000,
        }
    }
}

/// What travels over the socket. Clients send the envelope with the deadline
/// their own timeout implies; a bare request (no deadline) is still accepted.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum WireRequest {
    Envelope {
        request: SessionRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline: Option<MonoTime>,
    },
    Bare(SessionRequest),
}

impl SessionRequest {
    /// Variant name for logs (never the payload: `TypeText` carries text).
    fn kind(&self) -> &'static str {
        match self {
            Self::SessionInfo => "SessionInfo",
            Self::Shutdown => "Shutdown",
            Self::MovePointerScreenPoint { .. } => "MovePointerScreenPoint",
            Self::LeftMouseDown => "LeftMouseDown",
            Self::LeftMouseUp => "LeftMouseUp",
            Self::TypeText { .. } => "TypeText",
            Self::ClickScreenPoint { .. } => "ClickScreenPoint",
            Self::ScrollScreenPoint { .. } => "ScrollScreenPoint",
            Self::KeySequence { .. } => "KeySequence",
            Self::HoldKeyCodes { .. } => "HoldKeyCodes",
            Self::DragScreenPoints { .. } => "DragScreenPoints",
            Self::CaptureStillFrame { .. } => "CaptureStillFrame",
            Self::CaptureZoom { .. } => "CaptureZoom",
            Self::SetOverlayDisplay { .. } => "SetOverlayDisplay",
            Self::ReadClipboard => "ReadClipboard",
            Self::WriteClipboard { .. } => "WriteClipboard",
        }
    }

    /// Whether running this after its client gave up would inject input (or
    /// replace the clipboard) that nobody expects any more. These are refused
    /// once their deadline passed or their client is gone. `LeftMouseUp` is
    /// exempt: it only returns the button to neutral, and refusing a late one
    /// would leave the button held for the rest of the session.
    fn injects_input(&self) -> bool {
        matches!(
            self,
            Self::MovePointerScreenPoint { .. }
                | Self::LeftMouseDown
                | Self::TypeText { .. }
                | Self::ClickScreenPoint { .. }
                | Self::ScrollScreenPoint { .. }
                | Self::KeySequence { .. }
                | Self::HoldKeyCodes { .. }
                | Self::DragScreenPoints { .. }
                | Self::WriteClipboard { .. }
        )
    }
}

/// Why an input request must not run now, if it must not. A request is only
/// read once its client half-closed the socket, but it may sit in the listen
/// backlog for long before that (daemon stopped, busy, or in its consent
/// dialog); by then the client may have timed out and gone.
fn stale_input_refusal(
    request: &SessionRequest,
    deadline: Option<MonoTime>,
    now: MonoTime,
    peer_gone: impl FnOnce() -> bool,
) -> Option<String> {
    if !request.injects_input() {
        return None;
    }
    if deadline.is_some_and(|deadline| now > deadline) {
        return Some(format!(
            "refused stale {}: its client's deadline passed before the daemon got to it",
            request.kind()
        ));
    }
    if peer_gone() {
        return Some(format!(
            "refused stale {}: its client disconnected before the daemon got to it",
            request.kind()
        ));
    }
    None
}

/// Whether the client end of `stream` is closed. Only POLLHUP/POLLERR count:
/// the client half-closes its write side after the request (`exchange`),
/// which raises POLLRDHUP on our end, never POLLHUP; a client that closed
/// the socket or died raises POLLHUP.
fn peer_gone(stream: &UnixStream) -> bool {
    let mut pollfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
    ready > 0 && pollfd.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Machine-readable class of `error` (`ErrorKind::as_wire`), so the
    /// client maps it to the contract exit code without parsing the text. A
    /// string, not the enum: an older client ignores the field and a kind it
    /// does not know reads as a generic failure instead of a decode error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
}

/// What a daemon probe found.
#[derive(Debug)]
enum DaemonState {
    /// No daemon: no socket, or a socket nobody answers on and no live pid.
    Down,
    /// A daemon process exists but does not answer yet (consent dialog open,
    /// or busy with a long request).
    Starting(DaemonProc),
    /// The daemon answers.
    Up(PortalSessionInfo),
}

async fn daemon_state(socket: &Path) -> DaemonState {
    if let DaemonReply::Reply(Ok(info)) =
        request_daemon::<PortalSessionInfo>(socket, &SessionRequest::SessionInfo, PING_TIMEOUT)
            .await
    {
        return DaemonState::Up(info);
    }
    match live_daemon() {
        Some(daemon) => DaemonState::Starting(daemon),
        None => DaemonState::Down,
    }
}

pub async fn start_session_daemon() -> Result<PortalSessionInfo> {
    // Starting a session is the user's answer to an earlier stop.
    crate::stop_marker::remove();
    let socket = socket_path()?;
    match daemon_state(&socket).await {
        DaemonState::Up(info) => {
            bail!("a portal session is already active: {}", info.session_id)
        }
        DaemonState::Starting(daemon) => {
            // A daemon is already coming up (its consent dialog is probably
            // open). Spawning another would stack a second dialog; wait for
            // this one instead.
            return wait_until_ready(
                &socket,
                || daemon.alive(),
                READY_TIMEOUT,
                &crate::stop_marker::path(),
            )
            .await;
        }
        DaemonState::Down => {}
    }
    clear_for_new_daemon(&socket, &crate::stop_marker::path()).await?;

    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    // The daemon's stderr goes to a log next to the socket (truncated per
    // start), so a portal or PipeWire failure inside the detached session can
    // be read after the fact; nothing else ever sees that stream.
    let log = log_path()
        .and_then(|path| std::fs::File::create(path).map_err(Into::into))
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut command = Command::new(current_exe);
    command
        .arg("serve-session")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().context("failed to spawn session daemon")?;
    // `try_wait` rather than kill(pid, 0): an exited-but-unreaped child is a
    // zombie, which still "exists" to kill(2).
    wait_until_ready(
        &socket,
        || child.try_wait().map(|s| s.is_none()).unwrap_or(true),
        READY_TIMEOUT,
        &crate::stop_marker::path(),
    )
    .await
}

/// Wait until the daemon answers. Gives up early when the daemon process dies
/// (consent declined or closed -> `serve-session` exits), else after `limit`,
/// leaving the daemon waiting on its dialog: a later `session-start` picks it
/// up instead of opening a second dialog, a `session-end` reaps it.
///
/// The probe is ONE connection held open: the daemon binds its socket before
/// the dialog, so connect succeeds and the reply arrives the moment the daemon
/// starts serving. Re-probing with fresh connections would queue one stale
/// request per poll in the listen backlog.
///
/// A daemon that exited because the portal closed its session (the user
/// stopped sharing before it became ready) left the stopped-by-user marker:
/// that is reported as `stopped_by_user` (exit 5), so the caller does not
/// share the screen again on its own.
async fn wait_until_ready(
    socket: &Path,
    mut alive: impl FnMut() -> bool,
    limit: Duration,
    marker: &Path,
) -> Result<PortalSessionInfo> {
    let deadline = Instant::now() + limit;
    loop {
        let probe = async {
            let stream = UnixStream::connect(socket).await?;
            exchange::<PortalSessionInfo>(stream, &SessionRequest::SessionInfo, None).await
        };
        tokio::pin!(probe);
        loop {
            tokio::select! {
                result = &mut probe => match result {
                    Ok(info) => return Ok(info),
                    // Socket not bound yet, or the daemon dropped the
                    // connection (exiting): re-check below.
                    Err(_) => break,
                },
                _ = tokio::time::sleep(READY_POLL) => {}
            }
            check_still_starting(&mut alive, deadline, limit, marker)?;
        }
        check_still_starting(&mut alive, deadline, limit, marker)?;
        tokio::time::sleep(READY_POLL).await;
    }
}

fn check_still_starting(
    alive: &mut impl FnMut() -> bool,
    deadline: Instant,
    limit: Duration,
    marker: &Path,
) -> Result<()> {
    if !alive() {
        if crate::stop_marker::exists_at(marker) {
            return Err(crate::stop_marker::stopped_error());
        }
        bail!(
            "session daemon exited before becoming ready (consent dialog declined or closed, \
             or the portal failed){}{}",
            daemon_log_tail()
                .map(|line| format!(": {line}"))
                .unwrap_or_default(),
            log_hint()
        );
    }
    if Instant::now() >= deadline {
        bail!(
            "still waiting for the portal consent dialog after {}s; the session daemon keeps \
             waiting - answer the dialog, then run session-start again, or run session-end{}",
            limit.as_secs(),
            log_hint()
        );
    }
    Ok(())
}

/// Stop the daemon if one is running (idempotent): ask it to shut down over
/// the socket, then make sure the process is actually gone. Bounded: a daemon
/// that is stopped, wedged or behind its consent dialog is signalled instead
/// of waited on, so an app quit can never hang here.
///
/// Contract v1 shape: `{"ok": true, "ended": <a daemon was there>, "daemon":
/// "exited" | "sigterm" | "sigkill" | "shutdown-requested" | "none"}`.
pub async fn stop_session_daemon() -> Result<Value> {
    let socket = socket_path()?;
    let daemon = live_daemon();
    let answered = matches!(
        request_daemon::<Value>(&socket, &SessionRequest::Shutdown, SHUTDOWN_REQUEST_TIMEOUT).await,
        DaemonReply::Reply(Ok(_))
    );
    let how = match daemon {
        Some(daemon) => {
            let how = reap_daemon(&daemon, answered).await;
            // A daemon that had to be killed left its files behind.
            std::fs::remove_file(&socket).ok();
            remove_pidfile_if(daemon.pid);
            how
        }
        None if answered => "shutdown-requested",
        None => "none",
    };
    // After the reaping, so a daemon that recorded a stop on its way out
    // does not leave the marker behind.
    crate::stop_marker::remove();
    Ok(session_end_result(how))
}

fn session_end_result(how: &str) -> Value {
    serde_json::json!({ "ok": true, "ended": how != "none", "daemon": how })
}

/// `forget-session`: end any running daemon through the normal session-end
/// path, then delete the stored portal restore token (and nothing else).
/// Without this, `session-end` + `session-start` silently replays the stored
/// grant, so the consent dialog never shows again. Makes no D-Bus or portal
/// calls and creates no directories, so it works with no KDE session running.
pub async fn forget_session() -> Result<Value> {
    let mut result = stop_session_daemon().await?;
    let store = crate::token_store::TokenStore::locate()?;
    result["token_removed"] = Value::Bool(store.remove()?);
    result["token_path"] = Value::String(store.path().display().to_string());
    Ok(result)
}

/// What `doctor` reports about the session daemon.
pub struct DaemonProbe {
    /// The daemon answered a ping within the budget.
    pub answering: bool,
    /// A daemon process exists (it may still be behind its consent dialog).
    pub pid: Option<i32>,
}

/// Ping the daemon without starting anything and without creating files.
/// The reply (session info, which carries the restore token) is dropped.
pub async fn probe_daemon(limit: Duration) -> DaemonProbe {
    let answering = match socket_path() {
        Ok(socket) => matches!(
            tokio::time::timeout(
                limit,
                request_daemon::<Value>(&socket, &SessionRequest::SessionInfo, limit),
            )
            .await,
            Ok(DaemonReply::Reply(Ok(_)))
        ),
        Err(_) => false,
    };
    DaemonProbe {
        answering,
        pid: live_daemon().map(|daemon| daemon.pid),
    }
}

/// Make sure the daemon process is gone. Returns how it ended.
async fn reap_daemon(daemon: &DaemonProc, answered: bool) -> &'static str {
    if answered && wait_for_exit(daemon, SHUTDOWN_GRACE).await {
        return "exited";
    }
    if !daemon.alive() {
        return "exited";
    }
    daemon.signal(libc::SIGTERM);
    if wait_for_exit(daemon, TERM_GRACE).await {
        return "sigterm";
    }
    daemon.kill_group();
    wait_for_exit(daemon, Duration::from_secs(1)).await;
    "sigkill"
}

/// Wait up to `grace` for the daemon to exit; true when it did.
async fn wait_for_exit(daemon: &DaemonProc, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !daemon.alive() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !daemon.alive()
}

/// A handle on the daemon process. Liveness and signals go through a pidfd,
/// so a pid reused after the daemon exited is never signalled. Kernels
/// without pidfd (< 5.3, below every supported distro) fall back to the pid.
#[derive(Debug)]
struct DaemonProc {
    pid: i32,
    pidfd: Option<OwnedFd>,
}

impl DaemonProc {
    /// A handle on `pid`, or `None` when no such process exists.
    fn open(pid: i32) -> Option<Self> {
        // SAFETY: pidfd_open takes a pid and flags and returns a new fd.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd >= 0 {
            // SAFETY: the kernel just handed us this fd and nothing else owns it.
            let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            return Some(Self {
                pid,
                pidfd: Some(pidfd),
            });
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOSYS) if pid_alive(pid) => Some(Self { pid, pidfd: None }),
            _ => None,
        }
    }

    /// Whether the process is still running (a zombie counts as exited).
    fn alive(&self) -> bool {
        let Some(pidfd) = &self.pidfd else {
            return pid_alive(self.pid);
        };
        // A pidfd turns readable when its process exits.
        let mut pollfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, zero timeout.
        match unsafe { libc::poll(&mut pollfd, 1, 0) } {
            0 => true,
            n if n > 0 => false,
            _ => pid_alive(self.pid),
        }
    }

    fn signal(&self, signal: i32) {
        match &self.pidfd {
            // SAFETY: plain signal delivery through the pidfd; no siginfo.
            Some(pidfd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            },
            // SAFETY: plain signal delivery to a pid verified to be ours.
            None => unsafe {
                libc::kill(self.pid, signal);
            },
        }
    }

    /// SIGKILL the daemon and, when it leads its own process group (detached
    /// via setsid), the whole group: its overlay children share it, and a
    /// SIGKILLed daemon would otherwise leave the session indicator on
    /// screen. The group is only signalled while the leader is confirmed
    /// alive through the pidfd - a live leader keeps its pgid from being
    /// reused. A foreground daemon shares its caller's group: pid only.
    fn kill_group(&self) {
        // SAFETY: getpgid only reads; kill on a pgid we verified is ours.
        if self.pidfd.is_some() && self.alive() && unsafe { libc::getpgid(self.pid) } == self.pid {
            unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        }
        self.signal(libc::SIGKILL);
    }
}

/// Whether `pid` is a live (non-zombie) process (pidfd-less fallback).
fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) only probes for existence/permission.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // A zombie still "exists" to kill(2) until its parent reaps it.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => proc_stat_state(&stat) != Some('Z'),
        Err(_) => false,
    }
}

/// The state letter of a `/proc/<pid>/stat` line (after the `(comm)` field,
/// which may itself contain spaces and parentheses).
fn proc_stat_state(stat: &str) -> Option<char> {
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

fn pid_is_daemon(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| is_daemon_cmdline(&cmdline))
}

/// The running daemon from the pidfile: the process must still be a session
/// daemon (pids get reused). Its cmdline is checked again after the pidfd is
/// open, so the handle cannot refer to a process that took the pid over in
/// between.
fn live_daemon() -> Option<DaemonProc> {
    let pid = read_pidfile()?;
    if !pid_is_daemon(pid) {
        return None;
    }
    let daemon = DaemonProc::open(pid)?;
    (pid_is_daemon(pid) && daemon.alive()).then_some(daemon)
}

fn read_pidfile() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_path().ok()?)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (pid > 1).then_some(pid)
}

/// `serve-session` (detached) or `session-start --foreground`.
fn is_daemon_cmdline(cmdline: &[u8]) -> bool {
    let has_arg = |arg: &[u8]| cmdline.split(|b| *b == 0).any(|part| part == arg);
    has_arg(b"serve-session") || (has_arg(b"session-start") && has_arg(b"--foreground"))
}

fn remove_pidfile_if(pid: i32) {
    if read_pidfile() == Some(pid)
        && let Ok(path) = pid_path()
    {
        std::fs::remove_file(path).ok();
    }
}

pub async fn prepare_session_socket() -> Result<PathBuf> {
    crate::stop_marker::remove();
    let socket = socket_path()?;
    match daemon_state(&socket).await {
        DaemonState::Up(info) => {
            bail!("a portal session is already active: {}", info.session_id)
        }
        DaemonState::Starting(daemon) => {
            bail!("a session daemon (pid {}) is already starting", daemon.pid)
        }
        DaemonState::Down => {}
    }
    clear_for_new_daemon(&socket, &crate::stop_marker::path()).await?;
    Ok(socket)
}

/// The last step before a new daemon starts: drop a stale socket, and clear
/// the stopped-by-user marker again. The callers cleared it before probing,
/// but a daemon whose session was closed during that probe records its stop
/// on the way out (and gives up its socket and pidfile, so the probe reads
/// "down"); left in place, that marker would refuse every command of the
/// session about to start.
async fn clear_for_new_daemon(socket: &Path, marker: &Path) -> Result<()> {
    cleanup_stale_socket(socket).await?;
    crate::stop_marker::remove_at(marker);
    Ok(())
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
    // The pidfile is what lets `session-start` tell "still in the consent
    // dialog" from "gone", and lets `session-end` reap a daemon that does not
    // answer.
    let pidfile = pid_path()?;
    let own_pid = std::process::id();
    std::fs::write(&pidfile, own_pid.to_string())
        .with_context(|| format!("failed to write pidfile `{}`", pidfile.display()))?;
    let cleanup_files = || {
        std::fs::remove_file(socket).ok();
        remove_pidfile_if(own_pid as i32);
    };
    eprintln!("[kwin-portal-bridge] session daemon pid {own_pid} waiting for the portal session");
    let session = match LivePortalSession::open().await {
        Ok(session) => session,
        Err(error) => {
            cleanup_files();
            return Err(error);
        }
    };
    eprintln!(
        "[kwin-portal-bridge] portal session ready: {}",
        session_summary(&session.info())
    );
    let overlay = match SessionOverlayProcess::spawn(None) {
        Ok(overlay) => overlay,
        Err(error) => {
            bounded_portal_shutdown(session).await;
            cleanup_files();
            return Err(error);
        }
    };
    let teach_overlay = match TeachOverlayProcess::spawn() {
        Ok(teach_overlay) => teach_overlay,
        Err(error) => {
            let mut overlay = overlay;
            overlay.shutdown();
            bounded_portal_shutdown(session).await;
            cleanup_files();
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
    // Files are removed only while they are still this daemon's: after a
    // stop by the user they go early, and a new `session-start` may already
    // have put its own in place while this teardown runs.
    let socket_identity = file_identity(&socket);
    let mut closed = session.take_closed_signal();
    if closed.is_none() {
        eprintln!(
            "[kwin-portal-bridge] not watching for a stop from the screen-sharing indicator \
             (no Closed subscription)"
        );
    }
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
                    eprintln!("[kwin-portal-bridge] SIGTERM");
                    break;
                }
                _ = wait_closed(&mut closed) => {
                    record_stop_by_user(&socket, socket_identity);
                    break;
                }
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.context("failed to accept IPC client")?;
                    // A misbehaving client must never take down the session:
                    // the daemon's own stale-socket probe connects and
                    // immediately hangs up, and any caller can send garbage or
                    // stall mid-request. Time-limit both directions and log
                    // instead of propagating.
                    let (request, deadline) = match tokio::time::timeout(IPC_IO_TIMEOUT, read_request(&mut stream)).await {
                        Ok(Ok(request)) => request,
                        Ok(Err(error)) => {
                            eprintln!("[kwin-portal-bridge] ignoring invalid IPC request: {error:#}");
                            continue;
                        }
                        Err(_) => {
                            eprintln!("[kwin-portal-bridge] IPC client timed out sending its request");
                            continue;
                        }
                    };
                    if let Some(reason) =
                        stale_input_refusal(&request, deadline, MonoTime::now(), || peer_gone(&stream))
                    {
                        eprintln!("[kwin-portal-bridge] {reason}");
                        let response = SessionResponse { ok: false, result: None, error: Some(reason), error_kind: None };
                        // The client is most likely gone; nothing to log if so.
                        let _ = tokio::time::timeout(IPC_IO_TIMEOUT, write_response(&mut stream, response)).await;
                        continue;
                    }
                    // SIGTERM stays live during a request: every wait inside
                    // one is async (PipeWire waits poll), so the request is
                    // cancelled at its next await and the teardown below runs
                    // within its own limit instead of waiting for SIGKILL.
                    // A stop by the user does too: the request is abandoned
                    // and its caller gets `stopped_by_user`.
                    let handled = tokio::select! {
                        handled = handle_request(&mut session, &mut overlay, request) => Ok(handled),
                        _ = &mut terminate => Err(Interrupt::Terminate),
                        _ = wait_closed(&mut closed) => Err(Interrupt::Closed),
                    };
                    let (response, should_shutdown, stopped) = match handled {
                        Ok((response, should_shutdown)) => {
                            // A request that failed because the session was
                            // just closed reports the stop, not the symptom.
                            if !response.ok
                                && response.error_kind.is_none()
                                && closed.is_some()
                                && tokio::time::timeout(CLOSED_GRACE, wait_closed(&mut closed)).await.is_ok()
                            {
                                (stopped_by_user_response(), true, true)
                            } else {
                                (response, should_shutdown, false)
                            }
                        }
                        Err(Interrupt::Closed) => (stopped_by_user_response(), true, true),
                        Err(Interrupt::Terminate) => {
                            eprintln!("[kwin-portal-bridge] SIGTERM during a request; abandoning it");
                            break;
                        }
                    };
                    if stopped {
                        // The marker goes first, so a caller that retries
                        // right after this reply already sees it.
                        record_stop_by_user(&socket, socket_identity);
                    }
                    match tokio::time::timeout(IPC_IO_TIMEOUT, write_response(&mut stream, response)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            eprintln!("[kwin-portal-bridge] failed to write IPC response: {error:#}");
                        }
                        Err(_) => {
                            eprintln!("[kwin-portal-bridge] IPC client timed out reading the response");
                        }
                    }
                    if should_shutdown {
                        break;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Teardown stays best-effort, but a failed portal-session shutdown is
    // exactly the orphaned-session mode this daemon exists to avoid, so at
    // least leave a trace of what went wrong. It is also bounded: past
    // `TEARDOWN_LIMIT` a watchdog hard-exits, so a KWin or portal call that
    // never returns cannot keep the daemon (and the screen-sharing
    // indicator) alive after `session-end`. Overlays go first: they are
    // child processes that would outlive a hard exit.
    eprintln!("[kwin-portal-bridge] session daemon tearing down");
    arm_teardown_watchdog(socket.clone(), socket_identity);
    teach_overlay.shutdown();
    overlay.shutdown();
    if let Err(error) = restore_prepare_state() {
        eprintln!("[kwin-portal-bridge] failed to restore hidden windows on shutdown: {error:#}");
    }
    bounded_portal_shutdown(session).await;
    remove_socket_if_own(&socket, socket_identity);
    remove_pidfile_if(std::process::id() as i32);
    serve_result
}

/// What cut a request short.
enum Interrupt {
    Terminate,
    Closed,
}

/// Resolves when the portal closed the session; never without a watch.
async fn wait_closed(closed: &mut Option<ClosedSignal>) {
    match closed {
        Some(closed) => closed.closed().await,
        None => std::future::pending().await,
    }
}

/// The user stopped screen sharing (portal `Session::Closed`). Record it in
/// the marker, then drop the socket and pidfile at once, so no client queues
/// on a daemon that is going away; the caller then runs the normal teardown.
fn record_stop_by_user(socket: &Path, socket_identity: Option<FileIdentity>) {
    match crate::stop_marker::write() {
        Ok(path) => eprintln!(
            "[kwin-portal-bridge] the portal closed the session (screen sharing stopped by the \
             user or the desktop); recorded in {}",
            path.display()
        ),
        Err(error) => eprintln!(
            "[kwin-portal-bridge] the portal closed the session (screen sharing stopped by the \
             user or the desktop); failed to record it: {error:#}"
        ),
    }
    remove_socket_if_own(socket, socket_identity);
    remove_pidfile_if(std::process::id() as i32);
}

fn stopped_by_user_response() -> SessionResponse {
    SessionResponse {
        ok: false,
        result: None,
        error: Some(crate::stop_marker::STOPPED_MESSAGE.to_owned()),
        error_kind: Some(ErrorKind::StoppedByUser.as_wire().to_owned()),
    }
}

/// Device and inode of a file, to recognise "still the one we created".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

fn file_identity(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

/// Remove the session socket only while it is still the one this daemon
/// bound; one it could not identify is left for the next `session-start`'s
/// stale-socket cleanup.
fn remove_socket_if_own(socket: &Path, own: Option<FileIdentity>) {
    if own.is_some() && file_identity(socket) == own {
        std::fs::remove_file(socket).ok();
    }
}

async fn bounded_portal_shutdown(session: LivePortalSession) {
    match tokio::time::timeout(PORTAL_SHUTDOWN_TIMEOUT, session.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("[kwin-portal-bridge] failed to shut down portal session: {error:#}");
        }
        Err(_) => eprintln!(
            "[kwin-portal-bridge] portal session shutdown did not finish within {}ms",
            PORTAL_SHUTDOWN_TIMEOUT.as_millis()
        ),
    }
}

/// Hard-exit if the teardown that follows does not finish in time.
fn arm_teardown_watchdog(socket: PathBuf, socket_identity: Option<FileIdentity>) {
    let spawned = std::thread::Builder::new()
        .name("teardown-watchdog".to_owned())
        .spawn(move || {
            std::thread::sleep(TEARDOWN_LIMIT);
            eprintln!(
                "[kwin-portal-bridge] teardown exceeded {}ms; exiting hard",
                TEARDOWN_LIMIT.as_millis()
            );
            remove_socket_if_own(&socket, socket_identity);
            remove_pidfile_if(std::process::id() as i32);
            // SAFETY: _exit skips atexit handlers and destructors on purpose:
            // whatever is stuck must not be able to block the exit.
            unsafe { libc::_exit(1) };
        });
    if let Err(error) = spawned {
        eprintln!("[kwin-portal-bridge] failed to arm the teardown watchdog: {error}");
    }
}

/// Send one request to the session daemon. No daemon (no socket, or a stale
/// socket nobody listens on) is "no active portal session"; a daemon that is
/// there but fails or does not answer in time returns that error - daemon-side
/// errors verbatim, transport errors with the daemon log path.
///
/// While the stopped-by-user marker exists every request fails with
/// `stopped_by_user` (exit 5) without touching the socket; a daemon that went
/// away mid-request because the user stopped sharing reports the same.
pub async fn request<T: DeserializeOwned>(request: SessionRequest) -> Result<T> {
    crate::stop_marker::refuse_if_stopped()?;
    let socket = socket_path()?;
    let timeout = request_timeout(&request);
    let result = match request_daemon(&socket, &request, timeout).await {
        DaemonReply::Reply(result) => result,
        // The text stays: executors below contract 1 match on it.
        DaemonReply::NotRunning => Err(kinded(ErrorKind::NoSession, "no active portal session")),
    };
    result.map_err(|error| {
        if kind_of(&error).is_none_or(|kind| kind == ErrorKind::NoSession)
            && crate::stop_marker::exists()
        {
            crate::stop_marker::stopped_error()
        } else {
            error
        }
    })
}

/// Client-side budget for one daemon request. It must cover the daemon's own
/// worst case - a fresh PipeWire stream (create <= 10 s, first frame <= 10 s)
/// plus the capture settle, or a clipboard round trip (<= 5 s) - and stay
/// below the JS executor's 30 s execFile timeout for portal commands, so a
/// stuck daemon surfaces as our error (with the log path) instead of a killed
/// process. Requests that take time by design get it on top.
fn request_timeout(request: &SessionRequest) -> Duration {
    let extra_ms = match request {
        SessionRequest::HoldKeyCodes { duration_ms, .. } => *duration_ms,
        // Every character is a press + release over D-Bus plus the delay.
        SessionRequest::TypeText { text, delay_ms } => delay_ms
            .saturating_add(TYPE_PER_CHAR_ALLOWANCE_MS)
            .saturating_mul(text.chars().count() as u64),
        SessionRequest::KeySequence { keycodes, repeat } => u64::from(*repeat)
            .saturating_mul(keycodes.len() as u64)
            .saturating_mul(TYPE_PER_CHAR_ALLOWANCE_MS),
        _ => 0,
    };
    REQUEST_BASE_TIMEOUT.saturating_add(Duration::from_millis(extra_ms))
}

/// Per-character allowance on top of the typing delay (two D-Bus calls).
/// Mirrored by `js/executor_linux.js` in claude-desktop-extra.
const TYPE_PER_CHAR_ALLOWANCE_MS: u64 = 5;

/// What a request to the session daemon found.
enum DaemonReply<T> {
    /// No daemon: no socket, or a stale socket nobody listens on.
    NotRunning,
    /// A daemon accepted the connection; this is its answer or the failure.
    Reply(Result<T>),
}

/// Whether a failed connect means "no daemon" rather than "a daemon that is
/// broken".
fn daemon_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Send one request to the daemon, bounded by `timeout` end to end.
async fn request_daemon<T: DeserializeOwned>(
    socket: &Path,
    request: &SessionRequest,
    timeout: Duration,
) -> DaemonReply<T> {
    if !socket.exists() {
        return DaemonReply::NotRunning;
    }
    let stream = match tokio::time::timeout(PING_TIMEOUT, UnixStream::connect(socket)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) if daemon_absent(&error) => return DaemonReply::NotRunning,
        Ok(Err(error)) => {
            return DaemonReply::Reply(Err(anyhow::Error::new(error).context(format!(
                "failed to connect to the session daemon at `{}`{}",
                socket.display(),
                log_hint()
            ))));
        }
        Err(_) => {
            return DaemonReply::Reply(Err(anyhow::anyhow!(
                "the session daemon at `{}` did not accept the connection within {}s{}",
                socket.display(),
                PING_TIMEOUT.as_secs(),
                log_hint()
            )));
        }
    };
    let deadline = MonoTime::now().after(timeout);
    match tokio::time::timeout(timeout, exchange(stream, request, Some(deadline))).await {
        Ok(result) => DaemonReply::Reply(result),
        Err(_) => {
            // The daemon refuses input requests past their deadline, so only
            // one it had already started can still take effect.
            let late = if request.injects_input() {
                "; if the daemon had already started this action it may still complete, \
                 a queued one is refused"
            } else {
                ""
            };
            DaemonReply::Reply(Err(anyhow::anyhow!(
                "the session daemon did not answer within {}ms (stopped, wedged, or still \
                 waiting for the consent dialog){late}{}",
                timeout.as_millis(),
                log_hint()
            )))
        }
    }
}

/// Write one request on a connected stream and read the reply. A daemon-side
/// failure (`ok: false`) is returned verbatim; transport failures carry the
/// daemon log path.
async fn exchange<T: DeserializeOwned>(
    mut stream: UnixStream,
    request: &SessionRequest,
    deadline: Option<MonoTime>,
) -> Result<T> {
    let payload = serde_json::to_vec(&WireRequest::Envelope {
        request: request.clone(),
        deadline,
    })?;
    stream
        .write_all(&payload)
        .await
        .with_context(|| format!("failed to write session IPC request{}", log_hint()))?;
    stream
        .shutdown()
        .await
        .with_context(|| format!("failed to finish writing session IPC request{}", log_hint()))?;

    let mut response_bytes = Vec::new();
    stream
        .read_to_end(&mut response_bytes)
        .await
        .with_context(|| format!("failed to read session IPC response{}", log_hint()))?;
    let response: SessionResponse = serde_json::from_slice(&response_bytes).with_context(|| {
        format!(
            "failed to decode session IPC response (daemon died mid-request?){}",
            log_hint()
        )
    })?;

    if !response.ok {
        let message = response
            .error
            .unwrap_or_else(|| "session IPC request failed".to_owned());
        // The daemon's classification travels next to the text; re-attach it
        // so `main` exits with the contract code for it.
        return Err(
            match response
                .error_kind
                .as_deref()
                .and_then(ErrorKind::from_wire)
            {
                Some(kind) => kinded(kind, message),
                None => anyhow::anyhow!(message),
            },
        );
    }

    let value = response.result.unwrap_or(Value::Null);
    serde_json::from_value(value).context("failed to decode session IPC payload")
}

/// The session for the daemon log: id and shared streams, never the restore
/// token (`session-start` no longer prints any of it).
fn session_summary(info: &PortalSessionInfo) -> String {
    let streams = info
        .streams
        .iter()
        .map(|stream| {
            format!(
                "stream-{} at {},{} {}x{}",
                stream.node_id,
                stream.position[0],
                stream.position[1],
                stream.size[0],
                stream.size[1]
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "session {} (restore token {}), shared: {}",
        info.session_id,
        if info.restore_token.is_some() {
            "issued"
        } else {
            "not issued"
        },
        if streams.is_empty() { "none" } else { &streams }
    )
}

/// "; see <daemon.log>" for transport-level daemon failures.
fn log_hint() -> String {
    log_path()
        .map(|path| format!("; see {}", path.display()))
        .unwrap_or_default()
}

/// The last non-empty line of the daemon log (why a daemon exited early).
fn daemon_log_tail() -> Option<String> {
    let log = std::fs::read_to_string(log_path().ok()?).ok()?;
    last_log_line(&log)
}

fn last_log_line(log: &str) -> Option<String> {
    let line = log.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    const MAX: usize = 300;
    Some(if line.chars().count() > MAX {
        format!("{}...", line.chars().take(MAX).collect::<String>())
    } else {
        line.to_owned()
    })
}

fn runtime_dir() -> PathBuf {
    crate::util::runtime_base().join("kwin-portal-bridge")
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir().join("session.sock"))
}

/// The daemon's pidfile, next to the socket.
fn pid_path() -> Result<PathBuf> {
    Ok(runtime_dir().join("session.pid"))
}

/// The detached daemon's stderr log, next to the socket.
pub fn log_path() -> Result<PathBuf> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create `{}`", dir.display()))?;
    Ok(dir.join("daemon.log"))
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

async fn read_request(stream: &mut UnixStream) -> Result<(SessionRequest, Option<MonoTime>)> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .context("failed to read session IPC request")?;
    let wire: WireRequest =
        serde_json::from_slice(&bytes).context("failed to decode session IPC request")?;
    Ok(match wire {
        WireRequest::Envelope { request, deadline } => (request, deadline),
        WireRequest::Bare(request) => (request, None),
    })
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
                    error_kind: None,
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
        SessionRequest::ReadClipboard => {
            respond_async(bounded("clipboard read", CLIPBOARD_TIMEOUT, read_clipboard()).await)
        }
        SessionRequest::WriteClipboard { text } => respond_async(
            bounded("clipboard write", CLIPBOARD_TIMEOUT, write_clipboard(text)).await,
        ),
    };

    (result, false)
}

/// Run `future` with a time limit, as an error when it expires. The future is
/// dropped; a `spawn_blocking` worker inside it finishes (or stays stuck) on
/// its own thread without holding up the daemon.
async fn bounded<T>(
    what: &str,
    limit: Duration,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(limit, future)
        .await
        .unwrap_or_else(|_| bail!("{what} did not finish within {}ms", limit.as_millis()))
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
            error_kind: None,
        },
        Err(error) => SessionResponse {
            ok: false,
            result: None,
            error: Some(format!("failed to serialize session response: {error}")),
            error_kind: None,
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
            error_kind: kind_of(&error).map(|kind| kind.as_wire().to_owned()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_socket(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kpb-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("session.sock")
    }

    fn session_info() -> PortalSessionInfo {
        PortalSessionInfo {
            session_id: "test-session".to_owned(),
            pipewire_fd: 3,
            restore_token: None,
            remote_desktop_session: None,
            streams: vec![],
        }
    }

    /// A daemon stand-in that answers every request with `response` after
    /// `delay`.
    fn answering_daemon(
        socket: &Path,
        delay: Duration,
        response: SessionResponse,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        let response = serde_json::to_vec(&response).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await.unwrap();
            stream.write_all(&response).await.unwrap();
        })
    }

    #[test]
    fn only_a_missing_listener_counts_as_no_daemon() {
        use std::io::{Error, ErrorKind};
        assert!(daemon_absent(&Error::from(ErrorKind::NotFound)));
        assert!(daemon_absent(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(!daemon_absent(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!daemon_absent(&Error::from(ErrorKind::WouldBlock)));
        assert!(!daemon_absent(&Error::from(ErrorKind::TimedOut)));
    }

    #[tokio::test]
    async fn no_socket_and_stale_socket_are_no_daemon() {
        let socket = temp_socket("stale");
        let reply: DaemonReply<Value> = request_daemon(
            &socket,
            &SessionRequest::SessionInfo,
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(reply, DaemonReply::NotRunning));
        // A socket file whose listener is gone (daemon crashed) = no daemon.
        {
            // A child forked by a concurrent test would hold the listener
            // open until its exec; keep spawns out of this window.
            let _guard = spawn_lock();
            drop(UnixListener::bind(&socket).unwrap());
        }
        assert!(socket.exists());
        let reply: DaemonReply<Value> = request_daemon(
            &socket,
            &SessionRequest::SessionInfo,
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(reply, DaemonReply::NotRunning));
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_daemon_that_does_not_answer_times_out_with_the_log_path() {
        let socket = temp_socket("hang");
        // Bound but never accepting: the consent-dialog phase, or a stopped
        // daemon.
        let _listener = UnixListener::bind(&socket).unwrap();
        let started = Instant::now();
        let reply: DaemonReply<Value> = request_daemon(
            &socket,
            &SessionRequest::LeftMouseUp,
            Duration::from_millis(200),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        match reply {
            DaemonReply::Reply(Err(error)) => {
                let message = format!("{error:#}");
                assert!(message.contains("did not answer within 200ms"), "{message}");
                assert!(message.contains("daemon.log"), "{message}");
            }
            _ => panic!("expected an error reply"),
        }
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_daemon_side_error_is_returned_verbatim() {
        let socket = temp_socket("err");
        let server = answering_daemon(
            &socket,
            Duration::ZERO,
            SessionResponse {
                ok: false,
                result: None,
                error: Some("stream 51 is not shared".to_owned()),
                error_kind: None,
            },
        );
        let reply: DaemonReply<Value> = request_daemon(
            &socket,
            &SessionRequest::LeftMouseUp,
            Duration::from_secs(2),
        )
        .await;
        match reply {
            DaemonReply::Reply(Err(error)) => {
                assert_eq!(format!("{error:#}"), "stream 51 is not shared")
            }
            _ => panic!("expected the daemon's error"),
        }
        server.await.unwrap();
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    async fn reply_kind(error_kind: Option<&str>) -> (Option<ErrorKind>, String) {
        let socket = temp_socket("kind");
        let server = answering_daemon(
            &socket,
            Duration::ZERO,
            SessionResponse {
                ok: false,
                result: None,
                error: Some("monitor DP-2 (0,0 1x1) is not shared".to_owned()),
                error_kind: error_kind.map(ToOwned::to_owned),
            },
        );
        let reply: DaemonReply<Value> = request_daemon(
            &socket,
            &SessionRequest::LeftMouseUp,
            Duration::from_secs(2),
        )
        .await;
        server.await.unwrap();
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
        match reply {
            DaemonReply::Reply(Err(error)) => (kind_of(&error), format!("{error:#}")),
            _ => panic!("expected the daemon's error"),
        }
    }

    #[tokio::test]
    async fn the_error_kind_survives_the_socket() {
        let (kind, text) = reply_kind(Some("not_shared")).await;
        assert_eq!(kind, Some(ErrorKind::NotShared));
        assert_eq!(text, "monitor DP-2 (0,0 1x1) is not shared");
        // An older daemon (no kind) or a newer kind: generic failure, same text.
        assert_eq!(reply_kind(None).await.0, None);
        assert_eq!(reply_kind(Some("something_newer")).await.0, None);
    }

    #[tokio::test]
    async fn a_request_cut_short_by_a_stop_reaches_the_client_as_exit_5() {
        let socket = temp_socket("stopped");
        let server = answering_daemon(&socket, Duration::ZERO, stopped_by_user_response());
        let error = exchange::<Value>(
            UnixStream::connect(&socket).await.unwrap(),
            &move_request(),
            None,
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert_eq!(kind_of(&error), Some(ErrorKind::StoppedByUser));
        assert_eq!(crate::error_kind::exit_code(kind_of(&error)), 5);
        assert_eq!(error.to_string(), crate::stop_marker::STOPPED_MESSAGE);
        std::fs::remove_file(&socket).ok();
    }

    #[test]
    fn only_the_socket_this_daemon_bound_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("session.sock");
        std::fs::write(&socket, "").unwrap();
        let own = file_identity(&socket);
        assert!(own.is_some());

        // A new daemon replaced it: left alone.
        std::fs::remove_file(&socket).unwrap();
        std::fs::write(dir.path().join("other"), "").unwrap();
        std::fs::rename(dir.path().join("other"), &socket).unwrap();
        remove_socket_if_own(&socket, own);
        assert!(socket.exists());
        // Unknown identity: left alone.
        remove_socket_if_own(&socket, None);
        assert!(socket.exists());
        // Its own: removed.
        remove_socket_if_own(&socket, file_identity(&socket));
        assert!(!socket.exists());
    }

    #[test]
    fn a_classified_daemon_error_is_sent_with_its_kind() {
        let wrapped = Err::<(), _>(kinded(ErrorKind::NotShared, "monitor X is not shared"))
            .context("click failed");
        let response = respond_async(wrapped);
        assert!(!response.ok);
        assert_eq!(response.error_kind.as_deref(), Some("not_shared"));
        assert_eq!(
            response.error.as_deref(),
            Some("click failed: monitor X is not shared")
        );
        let plain = respond_async(Err::<(), _>(anyhow::anyhow!("portal failed")));
        assert_eq!(plain.error_kind, None);
        // An older client reading a newer reply ignores the extra field.
        let wire = serde_json::to_value(&response).unwrap();
        assert_eq!(wire["error_kind"], "not_shared");
    }

    #[test]
    fn session_end_reports_whether_a_daemon_was_there() {
        assert_eq!(
            session_end_result("none"),
            serde_json::json!({ "ok": true, "ended": false, "daemon": "none" })
        );
        for how in ["exited", "sigterm", "sigkill", "shutdown-requested"] {
            let result = session_end_result(how);
            assert_eq!(result["ended"], true, "{how}");
            assert_eq!(result["daemon"], how);
        }
    }

    #[test]
    fn request_timeout_covers_sleeps_and_stays_under_the_js_budget() {
        let base = request_timeout(&SessionRequest::LeftMouseUp);
        assert!(base < Duration::from_secs(30) && base > Duration::from_secs(21));
        let hold = SessionRequest::HoldKeyCodes {
            keycodes: vec![30],
            duration_ms: 4000,
        };
        assert_eq!(request_timeout(&hold), Duration::from_secs(29));
        let typing = SessionRequest::TypeText {
            text: "x".repeat(1000),
            delay_ms: 12,
        };
        assert_eq!(request_timeout(&typing), Duration::from_secs(25 + 17));
    }

    /// A marker path that never exists.
    fn no_marker() -> PathBuf {
        std::env::temp_dir().join(format!("kpb-test-no-marker-{}", std::process::id()))
    }

    #[tokio::test]
    async fn a_daemon_closed_before_ready_reports_the_stop_not_a_failure() {
        let socket = temp_socket("ready-stopped");
        let marker = socket.parent().unwrap().join("stopped-by-user");
        std::fs::write(&marker, "1\n").unwrap();
        let error = wait_until_ready(&socket, || false, Duration::from_secs(5), &marker)
            .await
            .unwrap_err();
        assert_eq!(kind_of(&error), Some(ErrorKind::StoppedByUser));
        assert_eq!(crate::error_kind::exit_code(kind_of(&error)), 5);
        assert_eq!(error.to_string(), crate::stop_marker::STOPPED_MESSAGE);
        // Still alive: the marker alone does not end the wait.
        let error = wait_until_ready(&socket, || true, Duration::from_millis(400), &marker)
            .await
            .unwrap_err();
        assert_eq!(kind_of(&error), None);
        assert!(error.to_string().contains("still waiting"), "{error}");
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_stop_recorded_during_the_probe_is_cleared_before_the_new_daemon() {
        let socket = temp_socket("clear-before-spawn");
        let marker = socket.parent().unwrap().join("stopped-by-user");
        // The closed daemon left its marker; its socket is gone or stale.
        std::fs::write(&marker, "1\n").unwrap();
        std::fs::write(&socket, "").unwrap();
        clear_for_new_daemon(&socket, &marker).await.unwrap();
        assert!(!marker.exists(), "the marker would refuse the new session");
        assert!(!socket.exists(), "stale socket left behind");
        // Nothing to clear: still fine.
        clear_for_new_daemon(&socket, &marker).await.unwrap();
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn ready_wait_reports_a_dead_daemon_as_exited() {
        let socket = temp_socket("ready-dead");
        let error = wait_until_ready(&socket, || false, Duration::from_secs(5), &no_marker())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("session daemon exited before becoming ready"),
            "{error}"
        );
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn ready_wait_distinguishes_still_waiting_from_exited() {
        let socket = temp_socket("ready-wait");
        let _listener = UnixListener::bind(&socket).unwrap();
        let started = Instant::now();
        let error = wait_until_ready(&socket, || true, Duration::from_millis(400), &no_marker())
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            error
                .to_string()
                .starts_with("still waiting for the portal consent dialog"),
            "{error}"
        );
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn ready_wait_returns_once_the_daemon_starts_serving() {
        let socket = temp_socket("ready-ok");
        let server = answering_daemon(
            &socket,
            Duration::from_millis(500),
            SessionResponse {
                ok: true,
                result: Some(serde_json::to_value(session_info()).unwrap()),
                error: None,
                error_kind: None,
            },
        );
        let info = wait_until_ready(&socket, || true, Duration::from_secs(5), &no_marker())
            .await
            .unwrap();
        assert_eq!(info.session_id, "test-session");
        server.await.unwrap();
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    fn move_request() -> SessionRequest {
        SessionRequest::LeftMouseDown
    }

    #[test]
    fn an_expired_input_request_is_refused() {
        let now = MonoTime::now();
        let past = MonoTime {
            sec: now.sec - 1,
            nsec: now.nsec,
        };
        let refusal = stale_input_refusal(&move_request(), Some(past), now, || false).unwrap();
        assert!(refusal.contains("refused stale LeftMouseDown"), "{refusal}");
        assert!(refusal.contains("deadline passed"), "{refusal}");
        // In time, or no deadline sent (older client): runs.
        let future = now.after(Duration::from_secs(5));
        assert!(stale_input_refusal(&move_request(), Some(future), now, || false).is_none());
        assert!(stale_input_refusal(&move_request(), None, now, || false).is_none());
        // Not input: a late capture or mouse-up is harmless and still runs.
        for request in [SessionRequest::ReadClipboard, SessionRequest::LeftMouseUp] {
            assert!(stale_input_refusal(&request, Some(past), now, || true).is_none());
        }
    }

    #[test]
    fn mono_time_after_carries_nanoseconds() {
        let t = MonoTime {
            sec: 10,
            nsec: 900_000_000,
        };
        assert_eq!(
            t.after(Duration::from_millis(250)),
            MonoTime {
                sec: 11,
                nsec: 150_000_000
            }
        );
        assert!(t < t.after(Duration::from_nanos(1)));
    }

    #[tokio::test]
    async fn a_closed_peer_is_refused_and_a_half_closed_peer_is_served() {
        let (mut client, server) = UnixStream::pair().unwrap();
        // The client half-closes its write side after the request (exchange
        // does): still connected, the request must run.
        client.shutdown().await.unwrap();
        assert!(!peer_gone(&server));
        let now = MonoTime::now();
        assert!(stale_input_refusal(&move_request(), None, now, || peer_gone(&server)).is_none());
        // The client gave up and closed the socket: refused.
        drop(client);
        // A child forked meanwhile by a concurrent test (any module) holds a
        // copy of the client fd until its exec; allow for that.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !peer_gone(&server) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(peer_gone(&server));
        let refusal =
            stale_input_refusal(&move_request(), None, now, || peer_gone(&server)).unwrap();
        assert!(refusal.contains("disconnected"), "{refusal}");
    }

    #[tokio::test]
    async fn requests_carry_their_deadline_and_bare_requests_still_parse() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let deadline = MonoTime::now().after(Duration::from_secs(3));
        let sender = tokio::spawn(async move {
            let _ = exchange::<Value>(client, &move_request(), Some(deadline)).await;
        });
        let (request, got) = read_request(&mut server).await.unwrap();
        assert_eq!(request.kind(), "LeftMouseDown");
        assert_eq!(got, Some(deadline));
        drop(server);
        sender.await.unwrap();

        let bare: WireRequest =
            serde_json::from_str(r#"{"KeySequence":{"keycodes":[30],"repeat":1}}"#).unwrap();
        assert!(matches!(
            bare,
            WireRequest::Bare(SessionRequest::KeySequence { .. })
        ));
        let bare_unit: WireRequest = serde_json::from_str(r#""Shutdown""#).unwrap();
        assert!(matches!(
            bare_unit,
            WireRequest::Bare(SessionRequest::Shutdown)
        ));
    }

    #[test]
    fn daemon_cmdlines_are_recognized() {
        assert!(is_daemon_cmdline(
            b"/opt/kwin-portal-bridge\0serve-session\0--socket\0/run/x.sock\0"
        ));
        assert!(is_daemon_cmdline(
            b"kwin-portal-bridge\0session-start\0--foreground\0"
        ));
        assert!(!is_daemon_cmdline(b"kwin-portal-bridge\0session-start\0"));
        assert!(!is_daemon_cmdline(b"vim\0serve-session.txt\0"));
    }

    #[test]
    fn proc_stat_state_survives_odd_comm_names() {
        assert_eq!(proc_stat_state("42 (sleep) S 1 42 42"), Some('S'));
        assert_eq!(proc_stat_state("42 (a) b) (c) Z 1 42 42"), Some('Z'));
        assert_eq!(proc_stat_state("garbage"), None);
    }

    #[test]
    fn last_log_line_skips_blanks_and_truncates() {
        assert_eq!(
            last_log_line("a\nportal: cancelled\n\n"),
            Some("portal: cancelled".into())
        );
        assert_eq!(last_log_line(""), None);
        let long = "x".repeat(400);
        assert_eq!(last_log_line(&long).unwrap().chars().count(), 303);
    }

    static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serializes child spawns against tests that need a socket fd to be
    /// really closed (`Command::spawn` returns only after the child's exec).
    fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
        SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn spawn(command: &mut std::process::Command) -> std::process::Child {
        let _guard = spawn_lock();
        command.spawn().unwrap()
    }

    fn spawn_sleeper() -> std::process::Child {
        spawn(std::process::Command::new("sleep").arg("30"))
    }

    #[tokio::test]
    async fn reaping_escalates_to_sigterm_for_a_silent_daemon() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        let started = Instant::now();
        assert_eq!(
            reap_daemon(&DaemonProc::open(pid).unwrap(), false).await,
            "sigterm"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn reaping_kills_a_stopped_daemon_within_bounds() {
        // The real daemon handles SIGTERM itself, so while stopped the signal
        // stays pending. (A SIG_DFL SIGTERM would kill even a stopped
        // process.) Model that with SIGTERM ignored.
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        // SAFETY: async-signal-safe call between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
        let mut child = spawn(&mut command);
        let pid = child.id() as i32;
        // SAFETY: signal to our own child.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let started = Instant::now();
        assert_eq!(
            reap_daemon(&DaemonProc::open(pid).unwrap(), false).await,
            "sigkill"
        );
        let took = started.elapsed();
        assert!(
            took >= TERM_GRACE && took < TERM_GRACE + Duration::from_secs(1),
            "{took:?}"
        );
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn a_daemon_that_exits_after_answering_is_not_signalled() {
        let mut child = spawn(std::process::Command::new("sleep").arg("0.2"));
        let pid = child.id() as i32;
        assert_eq!(
            reap_daemon(&DaemonProc::open(pid).unwrap(), true).await,
            "exited"
        );
        child.wait().unwrap();
    }
}
