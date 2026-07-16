use std::f32::consts::TAU;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use iced::font;
use iced::time;
use iced::widget::{
    Space, button, canvas, column, container, rich_text, row, scrollable, span, text as plain_text,
};
use iced::{
    Alignment, Border, Color, Element, Font, Length, Point, Radians, Rectangle, Shadow, Size,
    Subscription, Task, Theme, Vector, border, window,
};
use iced_layershell::actions::ActionCallback;
use iced_layershell::application;
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer};
use iced_layershell::settings::{LayerShellSettings, Settings, StartMode};
use iced_layershell::to_layer_message;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kwin::KWinBackend;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const EDGE_MARGIN: f32 = 20.0;
const ARROW_SIZE: f32 = 10.0;
const ARROW_GAP: f32 = 16.0;
const BUBBLE_WIDTH: f32 = 420.0;
const BUBBLE_MIN_HEIGHT: f32 = 140.0;
const BUBBLE_MAX_HEIGHT: f32 = 460.0;
const BUBBLE_WORKING_HEIGHT: f32 = 96.0;
const BUBBLE_RADIUS: f32 = 16.0;
const ACTION_ROW_HEIGHT: f32 = 40.0;
const BUBBLE_TOP_SECTION_HEIGHT: f32 = 34.0;
const BUBBLE_VERTICAL_GAP: f32 = 14.0;
const BUBBLE_PREVIEW_PADDING_HEIGHT: f32 = 18.0;
const EXPLANATION_CHARS_PER_LINE: usize = 46;
const PREVIEW_CHARS_PER_LINE: usize = 56;
const SPINNER_PERIOD_SECONDS: f32 = 0.8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeachAnchorLogical {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeachStepPayload {
    pub explanation: String,
    #[serde(default)]
    pub next_preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_logical: Option<TeachAnchorLogical>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeachOverlayAction {
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum TeachOverlayRequest {
    ShowStep {
        payload: TeachStepPayload,
        display: Option<String>,
    },
    SetWorking,
    Hide,
    SetDisplay {
        display: String,
    },
    WaitEvent,
}

#[derive(Debug, Serialize, Deserialize)]
struct TeachOverlayResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub struct TeachOverlayProcess {
    child: Child,
    socket: PathBuf,
}

impl TeachOverlayProcess {
    pub fn spawn() -> Result<Self> {
        let socket = prepare_socket()?;
        let stderr_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path()?)
            .context("failed to open teach overlay log file")?;
        let current_exe =
            std::env::current_exe().context("failed to resolve current executable")?;
        let mut command = Command::new(current_exe);
        command
            .arg("serve-teach-overlay")
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_log));

        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command
            .spawn()
            .context("failed to spawn teach overlay controller")?;

        let deadline = Instant::now() + Duration::from_secs(3);
        while !socket.exists() {
            if let Ok(Some(status)) = child.try_wait() {
                bail!(
                    "teach overlay controller exited during startup ({status}); see `{}`",
                    log_path()?.display()
                );
            }
            if Instant::now() >= deadline {
                // Don't leave the still-running child behind as a zombie the
                // long-lived daemon will never reap.
                child.kill().ok();
                child.wait().ok();
                bail!("teach overlay controller did not create its socket in time");
            }
            thread::sleep(Duration::from_millis(50));
        }

        Ok(Self { child, socket })
    }

    pub fn shutdown(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            std::fs::remove_file(&self.socket).ok();
            return;
        }

        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_file(&self.socket).ok();
    }
}

pub fn serve(socket: PathBuf) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(&socket).ok();
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create teach socket directory `{}`",
                parent.display()
            )
        })?;
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind teach socket `{}`", socket.display()))?;
    eprintln!("[teach-overlay] listening on {}", socket.display());
    let _cleanup = SocketCleanup {
        path: socket.clone(),
    };
    let service = Arc::new(Mutex::new(TeachOverlayService::new()));

    for accepted in listener.incoming() {
        let service = Arc::clone(&service);
        match accepted {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(error) = handle_client(service, stream) {
                        eprintln!("[teach-overlay] client request failed: {error:#}");
                    }
                });
            }
            Err(error) => {
                return Err(error).context("failed to accept teach overlay client");
            }
        }
    }

    Ok(())
}

pub fn show_step(payload: TeachStepPayload, display: Option<String>) -> Result<TeachOverlayAction> {
    request(TeachOverlayRequest::ShowStep { payload, display })
}

pub fn set_working() -> Result<Value> {
    request(TeachOverlayRequest::SetWorking)
}

pub fn hide() -> Result<Value> {
    request(TeachOverlayRequest::Hide)
}

pub fn set_display(display: String) -> Result<Value> {
    request(TeachOverlayRequest::SetDisplay { display })
}

pub fn wait_event() -> Result<TeachOverlayAction> {
    request(TeachOverlayRequest::WaitEvent)
}

pub fn preview(
    payload: TeachStepPayload,
    display: Option<String>,
    working: bool,
    auto_exit_ms: Option<u64>,
) -> Result<()> {
    let mode = if working {
        TeachVisualMode::Working
    } else {
        TeachVisualMode::Step
    };
    let mut state = TeachOverlayState {
        display,
        mode,
        last_payload: Some(payload),
        resolved_payload: None,
        pending_step: None,
        event_waiters: Vec::new(),
    };
    rebuild_resolved_payload(&mut state);

    let shared = Arc::new(Mutex::new(state));
    let shutdown = Arc::new(AtomicBool::new(false));

    if let Some(auto_exit_ms) = auto_exit_ms {
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(auto_exit_ms));
            shutdown.store(true, Ordering::Relaxed);
        });
    }

    let output = shared
        .lock()
        .expect("teach overlay preview mutex poisoned")
        .display
        .clone();
    run_overlay(shared, shutdown, output.as_deref())
}

fn request<T: DeserializeOwned>(request: TeachOverlayRequest) -> Result<T> {
    let socket = socket_path()?;
    if !socket.exists() {
        bail!("no active teach overlay");
    }

    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to teach socket `{}`", socket.display()))?;
    let payload = serde_json::to_vec(&request)?;
    stream
        .write_all(&payload)
        .context("failed to write teach overlay request")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("failed to finish teach overlay request")?;

    let mut response_bytes = Vec::new();
    stream
        .read_to_end(&mut response_bytes)
        .context("failed to read teach overlay response")?;
    let response: TeachOverlayResponse = serde_json::from_slice(&response_bytes)
        .context("failed to decode teach overlay response")?;

    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "teach overlay request failed".to_owned())
        );
    }

    serde_json::from_value(response.result.unwrap_or(Value::Null))
        .context("failed to decode teach overlay payload")
}

pub fn socket_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    Ok(base.join("kwin-portal-bridge").join("teach-overlay.sock"))
}

fn log_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = base.join("kwin-portal-bridge");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create teach log directory `{}`", dir.display()))?;
    Ok(dir.join("teach-overlay.log"))
}

fn prepare_socket() -> Result<PathBuf> {
    let socket = socket_path()?;
    if socket.exists() {
        std::fs::remove_file(&socket).with_context(|| {
            format!("failed to remove stale teach socket `{}`", socket.display())
        })?;
    }
    Ok(socket)
}

fn handle_client(service: Arc<Mutex<TeachOverlayService>>, mut stream: UnixStream) -> Result<()> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .context("failed to read teach overlay request")?;
    let request: TeachOverlayRequest =
        serde_json::from_slice(&bytes).context("failed to decode teach overlay request")?;
    let response = handle_request(service, request);
    let payload = serde_json::to_vec(&response)?;
    stream
        .write_all(&payload)
        .context("failed to write teach overlay response")?;
    Ok(())
}

fn handle_request(
    service: Arc<Mutex<TeachOverlayService>>,
    request: TeachOverlayRequest,
) -> TeachOverlayResponse {
    match request {
        TeachOverlayRequest::ShowStep { payload, display } => {
            let receiver = match service.lock() {
                Ok(mut service) => service.show_step(payload, display),
                Err(_) => Err(anyhow::anyhow!("teach overlay service mutex poisoned")),
            };

            match receiver {
                Ok(receiver) => match receiver.recv() {
                    Ok(choice) => respond_ok(choice),
                    Err(error) => {
                        respond_err(anyhow::Error::new(error).context("teach step waiter closed"))
                    }
                },
                Err(error) => respond_err(error),
            }
        }
        TeachOverlayRequest::SetWorking => match service.lock() {
            Ok(mut service) => respond_async(service.set_working().map(|_| Value::Null)),
            Err(_) => respond_err(anyhow::anyhow!("teach overlay service mutex poisoned")),
        },
        TeachOverlayRequest::Hide => match service.lock() {
            Ok(mut service) => respond_async(service.hide().map(|_| Value::Null)),
            Err(_) => respond_err(anyhow::anyhow!("teach overlay service mutex poisoned")),
        },
        TeachOverlayRequest::SetDisplay { display } => match service.lock() {
            Ok(mut service) => respond_async(service.set_display(display).map(|_| Value::Null)),
            Err(_) => respond_err(anyhow::anyhow!("teach overlay service mutex poisoned")),
        },
        TeachOverlayRequest::WaitEvent => {
            let receiver = match service.lock() {
                Ok(service) => service.wait_event(),
                Err(_) => Err(anyhow::anyhow!("teach overlay service mutex poisoned")),
            };

            match receiver {
                Ok(receiver) => match receiver.recv() {
                    Ok(event) => respond_ok(event),
                    Err(error) => {
                        respond_err(anyhow::Error::new(error).context("teach event waiter closed"))
                    }
                },
                Err(error) => respond_err(error),
            }
        }
    }
}

fn respond_ok<T: Serialize>(value: T) -> TeachOverlayResponse {
    match serde_json::to_value(value) {
        Ok(result) => TeachOverlayResponse {
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => TeachOverlayResponse {
            ok: false,
            result: None,
            error: Some(format!(
                "failed to serialize teach overlay response: {error}"
            )),
        },
    }
}

fn respond_async<T: Serialize>(result: Result<T>) -> TeachOverlayResponse {
    match result {
        Ok(value) => respond_ok(value),
        Err(error) => respond_err(error),
    }
}

fn respond_err(error: anyhow::Error) -> TeachOverlayResponse {
    TeachOverlayResponse {
        ok: false,
        result: None,
        error: Some(format!("{error:#}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeachVisualMode {
    Hidden,
    Step,
    Working,
}

struct TeachOverlayState {
    display: Option<String>,
    mode: TeachVisualMode,
    last_payload: Option<TeachStepPayload>,
    resolved_payload: Option<TeachStepPayload>,
    pending_step: Option<mpsc::Sender<TeachOverlayAction>>,
    event_waiters: Vec<mpsc::Sender<TeachOverlayAction>>,
}

impl TeachOverlayState {
    fn snapshot(&self) -> TeachOverlaySnapshot {
        TeachOverlaySnapshot {
            mode: self.mode,
            payload: self.resolved_payload.clone(),
        }
    }
}

struct TeachOverlayService {
    shared: Arc<Mutex<TeachOverlayState>>,
    runner: Option<TeachOverlayRunner>,
}

impl TeachOverlayService {
    fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(TeachOverlayState {
                display: None,
                mode: TeachVisualMode::Hidden,
                last_payload: None,
                resolved_payload: None,
                pending_step: None,
                event_waiters: Vec::new(),
            })),
            runner: None,
        }
    }

    fn show_step(
        &mut self,
        payload: TeachStepPayload,
        display: Option<String>,
    ) -> Result<mpsc::Receiver<TeachOverlayAction>> {
        if let Some(display) = display {
            self.set_display(display)?;
        }

        let (sender, receiver) = mpsc::channel();
        {
            let mut state = self
                .shared
                .lock()
                .expect("teach overlay state mutex poisoned");
            // A superseded step is not a user abort — keep "exit" reserved for
            // the user actually leaving so overlapping controllers can tell
            // the difference.
            resolve_pending_locked(&mut state, "superseded");
            state.mode = TeachVisualMode::Step;
            state.last_payload = Some(payload);
            rebuild_resolved_payload(&mut state);
            state.pending_step = Some(sender);
        }
        self.ensure_runner()?;
        Ok(receiver)
    }

    fn set_working(&mut self) -> Result<()> {
        {
            let mut state = self
                .shared
                .lock()
                .expect("teach overlay state mutex poisoned");
            if state.last_payload.is_none() {
                return Ok(());
            }
            state.mode = TeachVisualMode::Working;
            rebuild_resolved_payload(&mut state);
        }
        self.ensure_runner()?;
        Ok(())
    }

    fn hide(&mut self) -> Result<()> {
        let mut state = self
            .shared
            .lock()
            .expect("teach overlay state mutex poisoned");
        resolve_pending_locked(&mut state, "exit");
        notify_waiters_locked(&mut state, "hidden");
        state.mode = TeachVisualMode::Hidden;
        state.last_payload = None;
        state.resolved_payload = None;
        Ok(())
    }

    fn set_display(&mut self, display: String) -> Result<()> {
        let (changed, should_restart, should_stop) = {
            let mut state = self
                .shared
                .lock()
                .expect("teach overlay state mutex poisoned");
            let changed = state.display.as_deref() != Some(display.as_str());
            state.display = Some(display);
            rebuild_resolved_payload(&mut state);
            (
                changed,
                state.mode != TeachVisualMode::Hidden,
                state.mode == TeachVisualMode::Hidden,
            )
        };

        if changed {
            if should_restart {
                self.restart_runner()?;
            } else if should_stop {
                self.stop_runner();
            }
        }

        Ok(())
    }

    fn wait_event(&self) -> Result<mpsc::Receiver<TeachOverlayAction>> {
        let (sender, receiver) = mpsc::channel();
        let mut state = self
            .shared
            .lock()
            .expect("teach overlay state mutex poisoned");
        state.event_waiters.push(sender);
        Ok(receiver)
    }

    fn ensure_runner(&mut self) -> Result<()> {
        // A runner whose thread already exited (e.g. Wayland/layer-shell init
        // failure) must not count as running, or every later show-step blocks
        // forever on a pending step no UI will ever resolve.
        if self
            .runner
            .as_ref()
            .is_some_and(|runner| !runner.join_handle.is_finished())
        {
            return Ok(());
        }
        self.stop_runner();

        let display = self
            .shared
            .lock()
            .expect("teach overlay state mutex poisoned")
            .display
            .clone();
        self.runner = Some(TeachOverlayRunner::start(Arc::clone(&self.shared), display));
        Ok(())
    }

    fn restart_runner(&mut self) -> Result<()> {
        self.stop_runner();
        self.ensure_runner()
    }

    fn stop_runner(&mut self) {
        if let Some(runner) = self.runner.take() {
            runner.stop();
        }
    }
}

impl Drop for TeachOverlayService {
    fn drop(&mut self) {
        {
            let mut state = self
                .shared
                .lock()
                .expect("teach overlay state mutex poisoned");
            resolve_pending_locked(&mut state, "exit");
            notify_waiters_locked(&mut state, "hidden");
            state.mode = TeachVisualMode::Hidden;
            state.last_payload = None;
            state.resolved_payload = None;
        }
        self.stop_runner();
    }
}

fn resolve_pending_locked(state: &mut TeachOverlayState, action: &str) {
    if let Some(sender) = state.pending_step.take() {
        let _ = sender.send(TeachOverlayAction {
            action: action.to_owned(),
        });
    }
}

fn notify_waiters_locked(state: &mut TeachOverlayState, action: &str) {
    let event = TeachOverlayAction {
        action: action.to_owned(),
    };
    for waiter in state.event_waiters.drain(..) {
        let _ = waiter.send(event.clone());
    }
}

fn rebuild_resolved_payload(state: &mut TeachOverlayState) {
    state.resolved_payload = match (&state.last_payload, state.mode) {
        (Some(payload), TeachVisualMode::Step | TeachVisualMode::Working) => {
            Some(resolve_payload(payload, state.display.as_deref()))
        }
        _ => None,
    };
}

fn resolve_payload(payload: &TeachStepPayload, display: Option<&str>) -> TeachStepPayload {
    let anchor_logical = match (payload.anchor_logical.as_ref(), display) {
        (Some(anchor), Some(display)) => localize_anchor(anchor, display),
        _ => None,
    };

    TeachStepPayload {
        explanation: payload.explanation.clone(),
        next_preview: payload.next_preview.clone(),
        anchor_logical,
    }
}

fn localize_anchor(anchor: &TeachAnchorLogical, display: &str) -> Option<TeachAnchorLogical> {
    let kwin = KWinBackend::new();
    let screen = kwin
        .list_screens()
        .ok()?
        .into_iter()
        .find(|screen| screen.id == display)?;
    Some(TeachAnchorLogical {
        x: anchor.x - screen.geometry.x,
        y: anchor.y - screen.geometry.y,
    })
}

struct TeachOverlayRunner {
    shutdown: Arc<AtomicBool>,
    join_handle: JoinHandle<()>,
}

impl TeachOverlayRunner {
    fn start(shared: Arc<Mutex<TeachOverlayState>>, display: Option<String>) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join_handle = thread::spawn(move || {
            if let Err(error) = run_overlay(shared, thread_shutdown, display.as_deref()) {
                eprintln!("[teach-overlay] runner exited with error: {error:#}");
            }
        });

        Self {
            shutdown,
            join_handle,
        }
    }

    fn stop(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // The UI thread only exits after closing its window; if the surface
        // never mapped (e.g. a nonexistent output name) it would never
        // terminate, and callers hold the service mutex while stopping — an
        // unbounded join here bricks the whole teach socket. Wait a bounded
        // time, then detach as a last resort.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.join_handle.is_finished() {
            if Instant::now() >= deadline {
                eprintln!("[teach-overlay] runner did not shut down in time; detaching its thread");
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.join_handle.join().ok();
    }
}

fn run_overlay(
    shared: Arc<Mutex<TeachOverlayState>>,
    shutdown: Arc<AtomicBool>,
    output: Option<&str>,
) -> Result<()> {
    let start_mode = match output {
        Some(output) => StartMode::TargetScreen(output.to_owned()),
        None => StartMode::Active,
    };

    application(
        move || TeachOverlayApp::new(Arc::clone(&shared), Arc::clone(&shutdown)),
        namespace,
        update,
        view,
    )
    .subscription(subscription)
    .style(app_style)
    .antialiasing(true)
    .settings(Settings {
        layer_settings: LayerShellSettings {
            size: Some((0, 0)),
            anchor: Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right,
            layer: Layer::Overlay,
            exclusive_zone: 0,
            keyboard_interactivity: KeyboardInteractivity::None,
            start_mode,
            events_transparent: false,
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
    .context("failed to run teach overlay")?;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TeachOverlaySnapshot {
    mode: TeachVisualMode,
    payload: Option<TeachStepPayload>,
}

struct TeachOverlayApp {
    shared: Arc<Mutex<TeachOverlayState>>,
    shutdown: Arc<AtomicBool>,
    snapshot: TeachOverlaySnapshot,
    viewport: Size,
    window_id: Option<window::Id>,
    spinner_angle: f32,
    last_tick: Option<Instant>,
}

impl TeachOverlayApp {
    fn new(shared: Arc<Mutex<TeachOverlayState>>, shutdown: Arc<AtomicBool>) -> Self {
        let snapshot = shared
            .lock()
            .expect("teach overlay state mutex poisoned")
            .snapshot();
        Self {
            shared,
            shutdown,
            snapshot,
            viewport: Size::new(0.0, 0.0),
            window_id: None,
            spinner_angle: 0.0,
            last_tick: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrowSide {
    None,
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone)]
struct BubbleLayout {
    body_width: f32,
    body_height: f32,
    root_left: f32,
    root_top: f32,
    root_width: f32,
    root_height: f32,
    arrow_side: ArrowSide,
    arrow_offset: f32,
}

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    Tick(Instant),
    WindowOpened(window::Id),
    WindowResized(Size),
    WindowMonitorSizeLoaded(Option<Size>),
    NextPressed,
    ExitPressed,
}

fn namespace() -> String {
    String::from("Claude Teach Bubble")
}

fn subscription(_: &TeachOverlayApp) -> Subscription<Message> {
    Subscription::batch(vec![
        time::every(Duration::from_millis(16)).map(Message::Tick),
        window::open_events().map(Message::WindowOpened),
        window::resize_events().map(|(_, size)| Message::WindowResized(size)),
    ])
}

fn update(app: &mut TeachOverlayApp, message: Message) -> Task<Message> {
    match message {
        Message::Tick(now) => {
            let mut tasks = Vec::new();
            let snapshot = app
                .shared
                .lock()
                .expect("teach overlay state mutex poisoned")
                .snapshot();

            if snapshot != app.snapshot {
                app.snapshot = snapshot;
                tasks.push(update_input_region_task(app));
            }

            if app.snapshot.mode == TeachVisualMode::Working {
                let delta = app
                    .last_tick
                    .map(|last| now.duration_since(last).as_secs_f32())
                    .unwrap_or(0.0);
                app.spinner_angle =
                    (app.spinner_angle + delta / SPINNER_PERIOD_SECONDS * TAU) % TAU;
            }
            app.last_tick = Some(now);

            if app.shutdown.load(Ordering::Relaxed)
                && let Some(window_id) = app.window_id
            {
                tasks.push(window::close(window_id));
            }

            Task::batch(tasks)
        }
        Message::WindowOpened(window_id) => {
            app.window_id = Some(window_id);
            Task::batch(vec![
                window::size(window_id).map(Message::WindowResized),
                window::monitor_size(window_id).map(Message::WindowMonitorSizeLoaded),
                update_input_region_task(app),
            ])
        }
        Message::WindowResized(size) => {
            app.viewport = size;
            update_input_region_task(app)
        }
        Message::WindowMonitorSizeLoaded(Some(size)) => {
            if app.viewport.width <= 1.0 || app.viewport.height <= 1.0 {
                app.viewport = size;
            }
            update_input_region_task(app)
        }
        Message::WindowMonitorSizeLoaded(None) => Task::none(),
        Message::NextPressed => {
            let mut state = app
                .shared
                .lock()
                .expect("teach overlay state mutex poisoned");
            resolve_pending_locked(&mut state, "next");
            Task::none()
        }
        Message::ExitPressed => {
            let mut state = app
                .shared
                .lock()
                .expect("teach overlay state mutex poisoned");
            if state.pending_step.is_some() {
                resolve_pending_locked(&mut state, "exit");
            } else {
                notify_waiters_locked(&mut state, "exit");
            }
            Task::none()
        }
        _ => Task::none(),
    }
}

fn update_input_region_task(app: &TeachOverlayApp) -> Task<Message> {
    let layout = app
        .snapshot
        .payload
        .as_ref()
        .and_then(|payload| bubble_layout(&app.snapshot, payload, app.viewport));

    Task::done(Message::SetInputRegion(ActionCallback::new(
        move |region| {
            if let Some(layout) = &layout {
                region.add(
                    layout.root_left.round() as i32,
                    layout.root_top.round() as i32,
                    layout.root_width.ceil() as i32,
                    layout.root_height.ceil() as i32,
                );
            }
        },
    )))
}

fn view(app: &TeachOverlayApp) -> Element<'_, Message> {
    let Some(payload) = app.snapshot.payload.as_ref() else {
        return Space::new().width(Length::Fill).height(Length::Fill).into();
    };

    let Some(layout) = bubble_layout(&app.snapshot, payload, app.viewport) else {
        return Space::new().width(Length::Fill).height(Length::Fill).into();
    };

    let bubble = bubble(&app.snapshot, payload, &layout, app.spinner_angle);

    column![
        Space::new().height(layout.root_top).width(Length::Fill),
        row![
            Space::new().width(layout.root_left).height(Length::Shrink),
            bubble,
            Space::new().width(Length::Fill).height(Length::Shrink)
        ]
        .height(layout.root_height),
        Space::new().height(Length::Fill).width(Length::Fill)
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn bubble(
    snapshot: &TeachOverlaySnapshot,
    payload: &TeachStepPayload,
    layout: &BubbleLayout,
    spinner_angle: f32,
) -> Element<'static, Message> {
    match layout.arrow_side {
        ArrowSide::Top => column![
            arrow_canvas(
                layout.arrow_side,
                layout.arrow_offset,
                layout.body_width,
                ARROW_SIZE
            ),
            bubble_body(snapshot, payload, layout, spinner_angle),
        ]
        .width(layout.root_width)
        .height(layout.root_height)
        .into(),
        ArrowSide::Bottom => column![
            bubble_body(snapshot, payload, layout, spinner_angle),
            arrow_canvas(
                layout.arrow_side,
                layout.arrow_offset,
                layout.body_width,
                ARROW_SIZE
            ),
        ]
        .width(layout.root_width)
        .height(layout.root_height)
        .into(),
        ArrowSide::Left => row![
            arrow_canvas(
                layout.arrow_side,
                layout.arrow_offset,
                ARROW_SIZE,
                layout.body_height
            ),
            bubble_body(snapshot, payload, layout, spinner_angle),
        ]
        .width(layout.root_width)
        .height(layout.root_height)
        .into(),
        ArrowSide::Right => row![
            bubble_body(snapshot, payload, layout, spinner_angle),
            arrow_canvas(
                layout.arrow_side,
                layout.arrow_offset,
                ARROW_SIZE,
                layout.body_height
            ),
        ]
        .width(layout.root_width)
        .height(layout.root_height)
        .into(),
        ArrowSide::None => bubble_body(snapshot, payload, layout, spinner_angle),
    }
}

fn bubble_body(
    snapshot: &TeachOverlaySnapshot,
    payload: &TeachStepPayload,
    layout: &BubbleLayout,
    spinner_angle: f32,
) -> Element<'static, Message> {
    let next_preview = normalize_overlay_text(&payload.next_preview);

    let body_content: Element<'static, Message> = if snapshot.mode == TeachVisualMode::Working {
        row![
            spinner(spinner_angle),
            plain_text("Working...")
                .size(14)
                .color(Color::from_rgb(0.396, 0.388, 0.345)),
            Space::new().width(Length::Fill).height(Length::Shrink),
            secondary_button("Exit", Message::ExitPressed),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    } else {
        let content = column![
            plain_text("Claude")
                .size(14)
                .color(Color::from_rgb(0.851, 0.467, 0.341)),
            overlay_markdown(
                &payload.explanation,
                14,
                Color::from_rgb(0.161, 0.149, 0.106),
            ),
        ]
        .spacing(10);

        let next_preview_block = if !next_preview.trim().is_empty() {
            Some(
                container(overlay_markdown(
                    &payload.next_preview,
                    12,
                    Color::from_rgb(0.396, 0.388, 0.345),
                ))
                .padding(8.0)
                .style(|_| container::Style {
                    border: Border {
                        width: 0.5,
                        color: Color::from_rgba(0.0, 0.0, 0.0, 0.08),
                        ..Border::default()
                    },
                    ..container::Style::default()
                }),
            )
        } else {
            None
        };

        let mut body = column![scrollable(content).height(Length::Fill).width(Length::Fill),]
            .spacing(14)
            .height(Length::Fill);

        if let Some(next_preview_block) = next_preview_block {
            body = body.push(next_preview_block);
        }

        body.push(
            row![
                Space::new().width(Length::Fill).height(Length::Shrink),
                secondary_button("Exit", Message::ExitPressed),
                primary_button("Next", Message::NextPressed),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .into()
    };

    container(body_content)
        .width(layout.body_width)
        .height(layout.body_height)
        .padding([18.0, 20.0])
        .style(|_| bubble_body_style())
        .into()
}

fn bubble_body_style() -> container::Style {
    container::Style {
        background: Some(Color::from_rgba(1.0, 0.98, 0.96, 0.98).into()),
        border: border::rounded(BUBBLE_RADIUS)
            .width(1.0)
            .color(Color::from_rgba(0.0, 0.0, 0.0, 0.08)),
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.16),
            offset: Vector::new(0.0, 10.0),
            blur_radius: 32.0,
        },
        ..container::Style::default()
    }
}

type RichSpan = iced::widget::text::Span<'static>;

#[derive(Clone, Copy, Default)]
struct InlineMarkdownStyle {
    bold: bool,
    italic: bool,
    code: bool,
}

fn overlay_markdown(content: &str, size: u32, color: Color) -> Element<'static, Message> {
    let mut lines = column![].spacing(8).width(Length::Fill);

    for raw_line in content.lines() {
        lines = lines.push(overlay_markdown_line(raw_line, size, color));
    }

    if content.trim().is_empty() {
        lines = lines.push(Space::new().width(Length::Fill).height(0));
    }

    lines.into()
}

fn overlay_markdown_line(
    raw_line: &str,
    base_size: u32,
    color: Color,
) -> Element<'static, Message> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return Space::new().width(Length::Fill).height(6).into();
    }

    let (heading_level, content) = if let Some(content) = trimmed.strip_prefix("### ") {
        (Some(3_u8), content)
    } else if let Some(content) = trimmed.strip_prefix("## ") {
        (Some(2_u8), content)
    } else if let Some(content) = trimmed.strip_prefix("# ") {
        (Some(1_u8), content)
    } else {
        (None, trimmed)
    };

    let mut spans = if let Some(content) = content
        .strip_prefix("- ")
        .or_else(|| content.strip_prefix("* "))
        .or_else(|| content.strip_prefix("+ "))
    {
        let mut spans = vec![span("• ")];
        spans.extend(parse_inline_markdown(content.trim()));
        spans
    } else if let Some(content) = content.strip_prefix("> ") {
        parse_inline_markdown(content.trim())
    } else if let Some((marker, content)) = split_ordered_list_item(content) {
        let mut spans = vec![span(format!("{marker} "))];
        spans.extend(parse_inline_markdown(content.trim()));
        spans
    } else {
        parse_inline_markdown(content)
    };

    let line_height = if let Some(level) = heading_level {
        style_heading_spans(&mut spans, base_size, level);
        match level {
            1 => 1.2,
            2 => 1.24,
            _ => 1.28,
        }
    } else {
        1.35
    };

    rich_text(spans)
        .size(base_size)
        .line_height(line_height)
        .color(color)
        .width(Length::Fill)
        .into()
}

fn parse_inline_markdown(content: &str) -> Vec<RichSpan> {
    let mut spans = Vec::new();
    let mut style = InlineMarkdownStyle::default();
    let mut buffer = String::new();
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        let current = chars[i];
        let next = chars.get(i + 1).copied();

        if current == '\\'
            && let Some(escaped) = next
        {
            buffer.push(escaped);
            i += 2;
            continue;
        }

        if current == '`' {
            flush_inline_markdown_buffer(&mut spans, &mut buffer, style);
            style.code = !style.code;
            i += 1;
            continue;
        }

        if !style.code && current == '*' && next == Some('*') {
            flush_inline_markdown_buffer(&mut spans, &mut buffer, style);
            style.bold = !style.bold;
            i += 2;
            continue;
        }

        if !style.code && current == '_' && next == Some('_') {
            flush_inline_markdown_buffer(&mut spans, &mut buffer, style);
            style.bold = !style.bold;
            i += 2;
            continue;
        }

        if !style.code && (current == '*' || current == '_') {
            flush_inline_markdown_buffer(&mut spans, &mut buffer, style);
            style.italic = !style.italic;
            i += 1;
            continue;
        }

        if !style.code && current == '~' && next == Some('~') {
            i += 2;
            continue;
        }

        buffer.push(current);
        i += 1;
    }

    flush_inline_markdown_buffer(&mut spans, &mut buffer, style);
    spans
}
#[allow(clippy::approx_constant)]
fn flush_inline_markdown_buffer(
    spans: &mut Vec<RichSpan>,
    buffer: &mut String,
    style: InlineMarkdownStyle,
) {
    if buffer.is_empty() {
        return;
    }

    let text = std::mem::take(buffer);
    let mut fragment = span(text);

    if style.code {
        fragment = fragment
            .font(Font::MONOSPACE)
            .color(Color::from_rgb(0.318, 0.259, 0.192))
            .background(Color::from_rgba(0.831, 0.776, 0.694, 0.45))
            .border(border::rounded(4.0))
            .padding([1.0, 4.0]);
    } else if style.bold || style.italic {
        fragment = fragment.font(Font {
            weight: if style.bold {
                font::Weight::Bold
            } else {
                font::Weight::Normal
            },
            style: if style.italic {
                font::Style::Italic
            } else {
                font::Style::Normal
            },
            ..Font::default()
        });
    }

    spans.push(fragment);
}

fn style_heading_spans(spans: &mut [RichSpan], base_size: u32, level: u8) {
    let heading_size = match level {
        1 => base_size + 8,
        2 => base_size + 5,
        _ => base_size + 3,
    };

    for fragment in spans {
        fragment.size = Some(heading_size.into());

        let existing = fragment.font.unwrap_or_default();
        fragment.font = Some(Font {
            weight: font::Weight::Bold,
            ..existing
        });
    }
}

fn primary_button(label: &'static str, message: Message) -> Element<'static, Message> {
    button(plain_text(label).size(13).color(Color::WHITE))
        .padding([8.0, 16.0])
        .style(|_, status| match status {
            button::Status::Hovered => button::Style {
                background: Some(Color::from_rgb(0.784, 0.416, 0.290).into()),
                text_color: Color::WHITE,
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            },
            button::Status::Pressed => button::Style {
                background: Some(Color::from_rgb(0.718, 0.369, 0.251).into()),
                text_color: Color::WHITE,
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            },
            button::Status::Disabled => button::Style {
                background: Some(Color::from_rgba(0.851, 0.467, 0.341, 0.5).into()),
                text_color: Color::WHITE,
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            },
            _ => button::Style {
                background: Some(Color::from_rgb(0.851, 0.467, 0.341).into()),
                text_color: Color::WHITE,
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            },
        })
        .on_press(message)
        .into()
}

fn secondary_button(label: &'static str, message: Message) -> Element<'static, Message> {
    button(
        plain_text(label)
            .size(13)
            .color(Color::from_rgb(0.239, 0.224, 0.161)),
    )
    .padding([8.0, 16.0])
    .style(|_, status| {
        let background = match status {
            button::Status::Hovered => Color::from_rgba(0.0, 0.0, 0.0, 0.04),
            button::Status::Pressed => Color::from_rgba(0.0, 0.0, 0.0, 0.08),
            _ => Color::TRANSPARENT,
        };

        button::Style {
            background: Some(background.into()),
            text_color: Color::from_rgb(0.239, 0.224, 0.161),
            border: Border {
                radius: 8.0.into(),
                width: 1.0,
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.12),
            },
            ..button::Style::default()
        }
    })
    .on_press(message)
    .into()
}

fn spinner(angle: f32) -> Element<'static, Message> {
    canvas::Canvas::new(SpinnerProgram { angle })
        .width(18.0)
        .height(18.0)
        .into()
}

fn arrow_canvas(
    side: ArrowSide,
    offset: f32,
    width: f32,
    height: f32,
) -> Element<'static, Message> {
    canvas::Canvas::new(ArrowProgram { side, offset })
        .width(width)
        .height(height)
        .into()
}

fn bubble_layout(
    snapshot: &TeachOverlaySnapshot,
    payload: &TeachStepPayload,
    viewport: Size,
) -> Option<BubbleLayout> {
    let viewport_width = viewport.width.max(0.0);
    let viewport_height = viewport.height.max(0.0);

    // Layer-shell windows report a transient tiny viewport before the real
    // fullscreen size arrives. Skip layout until we have sane bounds.
    if viewport_width <= EDGE_MARGIN * 2.0 || viewport_height <= EDGE_MARGIN * 2.0 {
        return None;
    }

    let body_width = BUBBLE_WIDTH.min((viewport_width - EDGE_MARGIN * 2.0).max(280.0));
    let body_height = estimated_body_height(snapshot, payload);

    let Some(anchor) = payload.anchor_logical.as_ref() else {
        let root_width = body_width;
        let root_height = body_height;
        return Some(BubbleLayout {
            body_width,
            body_height,
            root_left: ((viewport_width - root_width) * 0.5)
                .round()
                .max(EDGE_MARGIN),
            root_top: ((viewport_height - root_height) * 0.5)
                .round()
                .max(EDGE_MARGIN),
            root_width,
            root_height,
            arrow_side: ArrowSide::None,
            arrow_offset: 0.0,
        });
    };

    let ax = clamp_to_range(anchor.x as f32, EDGE_MARGIN, viewport_width - EDGE_MARGIN);
    let ay = clamp_to_range(anchor.y as f32, EDGE_MARGIN, viewport_height - EDGE_MARGIN);
    let fits_below = ay + ARROW_GAP + body_height + EDGE_MARGIN <= viewport_height;
    let fits_above = ay - ARROW_GAP - body_height - EDGE_MARGIN >= 0.0;
    let fits_right = ax + ARROW_GAP + body_width + EDGE_MARGIN <= viewport_width;
    let fits_left = ax - ARROW_GAP - body_width - EDGE_MARGIN >= 0.0;

    let side = if fits_below {
        ArrowSide::Top
    } else if fits_above {
        ArrowSide::Bottom
    } else if fits_right {
        ArrowSide::Left
    } else if fits_left {
        ArrowSide::Right
    } else {
        ArrowSide::Top
    };

    match side {
        ArrowSide::Top | ArrowSide::Bottom => {
            let body_left = (ax - body_width / 2.0).round().clamp(
                EDGE_MARGIN,
                (viewport_width - body_width - EDGE_MARGIN).max(EDGE_MARGIN),
            );
            let body_top = if side == ArrowSide::Top {
                (ay + ARROW_GAP).round()
            } else {
                (ay - ARROW_GAP - body_height).round()
            };
            let arrow_offset = clamp_to_range(
                ax - body_left,
                ARROW_SIZE + 6.0,
                body_width - ARROW_SIZE - 6.0,
            );

            Some(BubbleLayout {
                body_width,
                body_height,
                root_left: body_left,
                root_top: if side == ArrowSide::Top {
                    body_top - ARROW_SIZE
                } else {
                    body_top
                },
                root_width: body_width,
                root_height: body_height + ARROW_SIZE,
                arrow_side: side,
                arrow_offset,
            })
        }
        ArrowSide::Left | ArrowSide::Right => {
            let body_top = (ay - body_height / 2.0).round().clamp(
                EDGE_MARGIN,
                (viewport_height - body_height - EDGE_MARGIN).max(EDGE_MARGIN),
            );
            let body_left = if side == ArrowSide::Left {
                (ax + ARROW_GAP).round()
            } else {
                (ax - ARROW_GAP - body_width).round()
            };
            let arrow_offset = clamp_to_range(
                ay - body_top,
                ARROW_SIZE + 6.0,
                body_height - ARROW_SIZE - 6.0,
            );

            Some(BubbleLayout {
                body_width,
                body_height,
                root_left: if side == ArrowSide::Left {
                    body_left - ARROW_SIZE
                } else {
                    body_left
                },
                root_top: body_top,
                root_width: body_width + ARROW_SIZE,
                root_height: body_height,
                arrow_side: side,
                arrow_offset,
            })
        }
        ArrowSide::None => None,
    }
}

fn clamp_to_range(value: f32, min: f32, max: f32) -> f32 {
    if max < min {
        min
    } else {
        value.clamp(min, max)
    }
}

fn estimated_body_height(snapshot: &TeachOverlaySnapshot, payload: &TeachStepPayload) -> f32 {
    if snapshot.mode == TeachVisualMode::Working {
        return BUBBLE_WORKING_HEIGHT;
    }

    let explanation = normalize_overlay_text(&payload.explanation);
    let next_preview = normalize_overlay_text(&payload.next_preview);

    let explanation_lines = estimate_wrapped_lines(&explanation, EXPLANATION_CHARS_PER_LINE) as f32;
    let preview_lines = if next_preview.trim().is_empty() {
        0.0
    } else {
        estimate_wrapped_lines(&next_preview, PREVIEW_CHARS_PER_LINE) as f32
    };

    let preview_block = if preview_lines > 0.0 {
        BUBBLE_PREVIEW_PADDING_HEIGHT + preview_lines * 18.0
    } else {
        0.0
    };

    let heading_bonus = markdown_heading_bonus(&payload.explanation)
        + markdown_heading_bonus(&payload.next_preview);
    let explanation_block = explanation_lines * 24.0;
    let section_gaps = if preview_lines > 0.0 {
        BUBBLE_VERTICAL_GAP * 2.0
    } else {
        BUBBLE_VERTICAL_GAP
    };

    (BUBBLE_TOP_SECTION_HEIGHT
        + explanation_block
        + preview_block
        + ACTION_ROW_HEIGHT
        + section_gaps
        + 44.0
        + heading_bonus)
        .clamp(BUBBLE_MIN_HEIGHT, BUBBLE_MAX_HEIGHT)
}

fn markdown_heading_bonus(text: &str) -> f32 {
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("# ") {
                22.0
            } else if trimmed.starts_with("## ") {
                16.0
            } else if trimmed.starts_with("### ") {
                10.0
            } else {
                0.0
            }
        })
        .sum()
}

fn normalize_overlay_text(text: &str) -> String {
    let mut normalized = Vec::new();

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            normalized.push(String::new());
            continue;
        }

        if let Some(content) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            normalized.push(format!("• {}", strip_inline_markdown(content.trim())));
            continue;
        }

        if let Some(content) = trimmed.strip_prefix("> ") {
            normalized.push(strip_inline_markdown(content.trim()));
            continue;
        }

        if let Some((marker, content)) = split_ordered_list_item(trimmed) {
            normalized.push(format!(
                "{marker} {}",
                strip_inline_markdown(content.trim())
            ));
            continue;
        }

        normalized.push(strip_inline_markdown(trimmed));
    }

    normalized.join("\n")
}

fn split_ordered_list_item(line: &str) -> Option<(String, &str)> {
    let digit_count = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }

    let rest = &line[digit_count..];
    if let Some(content) = rest.strip_prefix(". ") {
        return Some((format!("{}.", &line[..digit_count]), content));
    }

    if let Some(content) = rest.strip_prefix(") ") {
        return Some((format!("{})", &line[..digit_count]), content));
    }

    None
}

fn strip_inline_markdown(text: &str) -> String {
    text.replace("**", "")
        .replace("__", "")
        .replace("~~", "")
        .replace(['`', '\\'], "")
}

fn estimate_wrapped_lines(text: &str, max_chars: usize) -> usize {
    let mut lines = 0usize;
    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            lines += 1;
            continue;
        }

        let mut current = 0usize;
        for word in trimmed.split_whitespace() {
            let word_len = word.chars().count();
            if current == 0 {
                current = word_len;
                continue;
            }

            if current + 1 + word_len > max_chars {
                lines += 1;
                current = word_len;
            } else {
                current += 1 + word_len;
            }
        }
        lines += usize::from(current > 0);
    }

    lines.max(1)
}

#[derive(Debug)]
struct ArrowProgram {
    side: ArrowSide,
    offset: f32,
}

impl<Message> canvas::Program<Message> for ArrowProgram {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let fill = Color::from_rgba(1.0, 0.98, 0.96, 0.98);
        let mut builder = canvas::path::Builder::new();

        match self.side {
            ArrowSide::Top => {
                builder.move_to(Point::new(self.offset - ARROW_SIZE, bounds.height));
                builder.line_to(Point::new(self.offset, 0.0));
                builder.line_to(Point::new(self.offset + ARROW_SIZE, bounds.height));
            }
            ArrowSide::Bottom => {
                builder.move_to(Point::new(self.offset - ARROW_SIZE, 0.0));
                builder.line_to(Point::new(self.offset, bounds.height));
                builder.line_to(Point::new(self.offset + ARROW_SIZE, 0.0));
            }
            ArrowSide::Left => {
                builder.move_to(Point::new(bounds.width, self.offset - ARROW_SIZE));
                builder.line_to(Point::new(0.0, self.offset));
                builder.line_to(Point::new(bounds.width, self.offset + ARROW_SIZE));
            }
            ArrowSide::Right => {
                builder.move_to(Point::new(0.0, self.offset - ARROW_SIZE));
                builder.line_to(Point::new(bounds.width, self.offset));
                builder.line_to(Point::new(0.0, self.offset + ARROW_SIZE));
            }
            ArrowSide::None => {}
        }

        builder.close();
        let path = builder.build();
        frame.fill(&path, fill);
        vec![frame.into_geometry()]
    }
}

#[derive(Debug)]
struct SpinnerProgram {
    angle: f32,
}

impl<Message> canvas::Program<Message> for SpinnerProgram {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let center = Point::new(bounds.width * 0.5, bounds.height * 0.5);
        let mut builder = canvas::path::Builder::new();
        builder.arc(canvas::path::Arc {
            center,
            radius: 6.0,
            start_angle: Radians(self.angle),
            end_angle: Radians(self.angle + TAU * 0.72),
        });
        let path = builder.build();
        frame.stroke(
            &path,
            canvas::Stroke::default()
                .with_color(Color::from_rgb(0.396, 0.388, 0.345))
                .with_width(2.5)
                .with_line_cap(canvas::LineCap::Round),
        );
        vec![frame.into_geometry()]
    }
}

fn app_style(_: &TeachOverlayApp, theme: &Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme.palette().text,
    }
}

struct SocketCleanup {
    path: PathBuf,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}
