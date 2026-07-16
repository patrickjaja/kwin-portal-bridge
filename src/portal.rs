use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{sync::mpsc as std_mpsc, time::Duration};

use anyhow::{Context, Result, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::CursorMode;
use ashpd::{AppID, register_host_app};
use lamco_pipewire::{
    PipeWireThreadCommand, PipeWireThreadManager, PixelFormat, SourceType as PwSourceType,
    StreamConfig as PwStreamConfig, StreamInfo as PwStreamInfo, StreamInfo, VideoFrame,
};
use lamco_portal::{PortalConfig, PortalManager, PortalSessionHandle};

use crate::daemon::{SessionRequest, request};
use crate::model::{
    ButtonStateResult, CapturedFrame, ClipboardReadResult, ClipboardWriteResult,
    PortalActionResult, PortalSessionInfo, PortalStream, ScreenInfo, ScreenshotCapture,
    ScreenshotResult, StreamSelection, TypeActionResult,
};
use crate::token_store::TokenStore;

const PORTAL_APP_ID: &str = "com.anthropic.Claude";

pub struct PortalBackend;
pub struct LivePortalSession {
    manager: PortalManager,
    session: PortalSessionHandle,
    restore_token: Option<String>,
    pipewire: Option<PersistentPipeWire>,
    held_buttons: HashSet<i32>,
    latest_frames: HashMap<u32, VideoFrame>,
}

struct PersistentPipeWire {
    manager: PipeWireThreadManager,
    active_streams: HashSet<u32>,
}

impl PortalBackend {
    pub fn new() -> Self {
        Self
    }

    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub async fn create_session(&self) -> Result<PortalSessionInfo> {
        request(SessionRequest::SessionInfo).await
    }

    pub async fn move_pointer_screen_point(
        &self,
        screen: &ScreenInfo,
        x: i32,
        y: i32,
    ) -> Result<PortalActionResult> {
        request(SessionRequest::MovePointerScreenPoint {
            screen: screen.clone(),
            x,
            y,
        })
        .await
    }

    pub async fn left_mouse_down(&self) -> Result<ButtonStateResult> {
        request(SessionRequest::LeftMouseDown).await
    }

    pub async fn left_mouse_up(&self) -> Result<ButtonStateResult> {
        request(SessionRequest::LeftMouseUp).await
    }

    pub async fn click_screen_point(
        &self,
        screen: &ScreenInfo,
        x: i32,
        y: i32,
        button: i32,
        count: u32,
        keycodes: &[i32],
    ) -> Result<PortalActionResult> {
        request(SessionRequest::ClickScreenPoint {
            screen: screen.clone(),
            x,
            y,
            button,
            count,
            keycodes: keycodes.to_vec(),
        })
        .await
    }

    pub async fn scroll_screen_point(
        &self,
        screen: &ScreenInfo,
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
    ) -> Result<PortalActionResult> {
        request(SessionRequest::ScrollScreenPoint {
            screen: screen.clone(),
            x,
            y,
            dx,
            dy,
        })
        .await
    }

    pub async fn drag_screen_points(
        &self,
        from_screen: &ScreenInfo,
        from_x: i32,
        from_y: i32,
        to_screen: &ScreenInfo,
        to_x: i32,
        to_y: i32,
    ) -> Result<PortalActionResult> {
        request(SessionRequest::DragScreenPoints {
            from_screen: from_screen.clone(),
            from_x,
            from_y,
            to_screen: to_screen.clone(),
            to_x,
            to_y,
        })
        .await
    }

    pub async fn key_sequence(&self, keycodes: &[i32], repeat: u32) -> Result<PortalActionResult> {
        request(SessionRequest::KeySequence {
            keycodes: keycodes.to_vec(),
            repeat,
        })
        .await
    }

    pub async fn type_text(&self, text: &str, delay_ms: u64) -> Result<TypeActionResult> {
        request(SessionRequest::TypeText {
            text: text.to_owned(),
            delay_ms,
        })
        .await
    }

    pub async fn read_clipboard(&self) -> Result<ClipboardReadResult> {
        request(SessionRequest::ReadClipboard).await
    }

    pub async fn write_clipboard(&self, text: &str) -> Result<ClipboardWriteResult> {
        request(SessionRequest::WriteClipboard {
            text: text.to_owned(),
        })
        .await
    }

    pub async fn hold_key_codes(
        &self,
        keycodes: &[i32],
        duration_ms: u64,
    ) -> Result<PortalActionResult> {
        request(SessionRequest::HoldKeyCodes {
            keycodes: keycodes.to_vec(),
            duration_ms,
        })
        .await
    }

    pub async fn capture_still_image(&self, screen: &ScreenInfo) -> Result<ScreenshotResult> {
        request(SessionRequest::CaptureStillFrame {
            screen: screen.clone(),
        })
        .await
    }

    pub async fn capture_zoom_image(
        &self,
        screen: &ScreenInfo,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<ScreenshotCapture> {
        request(SessionRequest::CaptureZoom {
            screen: screen.clone(),
            x,
            y,
            w,
            h,
        })
        .await
    }

    pub async fn set_overlay_display(&self, display: Option<&str>) -> Result<serde_json::Value> {
        let value: serde_json::Value = request(SessionRequest::SetOverlayDisplay {
            display: display.map(ToOwned::to_owned),
        })
        .await?;
        Ok(value)
    }

    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub async fn capture_raw_frame_for_screen(&self, screen: &ScreenInfo) -> Result<CapturedFrame> {
        let (manager, session, restore_token) = start_session().await?;
        let info = session_info(&session, restore_token);
        let target_stream = match_stream_to_screen(&info.streams, screen)?;
        let fd = dup_fd(session.pipewire_fd())?;
        let pw_stream = to_pipewire_stream(&target_stream);
        let frame_task =
            tokio::task::spawn_blocking(move || read_one_pipewire_frame(fd, pw_stream));

        let frame = frame_task
            .await
            .context("PipeWire frame worker task failed to join")??;

        manager.cleanup().await.ok();
        drop(session);

        Ok(CapturedFrame { frame })
    }
}

impl LivePortalSession {
    pub async fn open() -> Result<Self> {
        let (manager, session, restore_token) = start_session().await?;
        Ok(Self {
            manager,
            session,
            restore_token,
            pipewire: None,
            held_buttons: HashSet::new(),
            latest_frames: HashMap::new(),
        })
    }

    pub fn info(&self) -> PortalSessionInfo {
        session_info(&self.session, self.restore_token.clone())
    }

    pub fn drain_capture_backlog(&mut self) -> usize {
        match self.pipewire.as_mut() {
            Some(pipewire) => drain_pending_frames(&pipewire.manager, &mut self.latest_frames),
            None => 0,
        }
    }

    pub async fn shutdown(mut self) -> Result<()> {
        for button in self.held_buttons.clone() {
            self.manager
                .remote_desktop()
                .notify_pointer_button(self.session.ashpd_session(), button, false)
                .await
                .ok();
        }
        if let Some(mut pipewire) = self.pipewire.take() {
            pipewire.manager.shutdown().ok();
        }
        self.manager.cleanup().await.ok();
        drop(self.session);
        Ok(())
    }

    pub async fn left_mouse_down(&mut self) -> Result<ButtonStateResult> {
        let button = 272;
        if self.held_buttons.contains(&button) {
            bail!("left mouse button is already held");
        }

        self.manager
            .remote_desktop()
            .notify_pointer_button(self.session.ashpd_session(), button, true)
            .await
            .context("failed to press left mouse button through the portal")?;

        self.held_buttons.insert(button);

        Ok(ButtonStateResult {
            action: "left-mouse-down".to_owned(),
            button: "left".to_owned(),
            is_held: true,
            was_held: false,
        })
    }

    pub async fn left_mouse_up(&mut self) -> Result<ButtonStateResult> {
        let button = 272;
        let was_held = self.held_buttons.remove(&button);

        if was_held {
            self.manager
                .remote_desktop()
                .notify_pointer_button(self.session.ashpd_session(), button, false)
                .await
                .context("failed to release left mouse button through the portal")?;
        }

        Ok(ButtonStateResult {
            action: "left-mouse-up".to_owned(),
            button: "left".to_owned(),
            is_held: false,
            was_held,
        })
    }

    pub async fn click_screen_point(
        &mut self,
        screen: &ScreenInfo,
        x: i32,
        y: i32,
        button: i32,
        count: u32,
        keycodes: &[i32],
    ) -> Result<PortalActionResult> {
        if self.held_buttons.contains(&button) {
            bail!("button {button} is currently held by the session");
        }

        let info = self.info();
        let target_stream = match_stream_to_screen(&info.streams, screen)?;
        let (local_x, local_y) = local_stream_point(screen, &target_stream, x, y)?;

        self.manager
            .remote_desktop()
            .notify_pointer_motion_absolute(
                self.session.ashpd_session(),
                target_stream.stream.node_id,
                local_x,
                local_y,
            )
            .await
            .context("failed to move pointer before click")?;

        self.press_keycodes(keycodes).await?;

        let click_result = async {
            for _ in 0..count.max(1) {
                self.manager
                    .remote_desktop()
                    .notify_pointer_button(self.session.ashpd_session(), button, true)
                    .await
                    .context("failed to send pointer press through the portal")?;
                self.manager
                    .remote_desktop()
                    .notify_pointer_button(self.session.ashpd_session(), button, false)
                    .await
                    .context("failed to send pointer release through the portal")?;
            }

            Ok::<(), anyhow::Error>(())
        }
        .await;

        let release_result = self.release_keycodes(keycodes).await;
        click_result?;
        release_result?;

        Ok(PortalActionResult {
            action: "click".to_owned(),
            session: info,
            target_stream: Some(target_stream),
        })
    }

    pub async fn move_pointer_screen_point(
        &mut self,
        screen: &ScreenInfo,
        x: i32,
        y: i32,
    ) -> Result<PortalActionResult> {
        let info = self.info();
        let target_stream = match_stream_to_screen(&info.streams, screen)?;
        let (local_x, local_y) = local_stream_point(screen, &target_stream, x, y)?;

        self.manager
            .remote_desktop()
            .notify_pointer_motion_absolute(
                self.session.ashpd_session(),
                target_stream.stream.node_id,
                local_x,
                local_y,
            )
            .await
            .context("failed to move pointer through the portal")?;

        Ok(PortalActionResult {
            action: "mouse-move".to_owned(),
            session: info,
            target_stream: Some(target_stream),
        })
    }

    pub async fn scroll_screen_point(
        &mut self,
        screen: &ScreenInfo,
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
    ) -> Result<PortalActionResult> {
        let info = self.info();
        let target_stream = match_stream_to_screen(&info.streams, screen)?;
        let (local_x, local_y) = local_stream_point(screen, &target_stream, x, y)?;

        self.manager
            .remote_desktop()
            .notify_pointer_motion_absolute(
                self.session.ashpd_session(),
                target_stream.stream.node_id,
                local_x,
                local_y,
            )
            .await
            .context("failed to move pointer before scroll")?;

        self.manager
            .remote_desktop()
            .notify_pointer_axis(self.session.ashpd_session(), dx, dy)
            .await
            .context("failed to send scroll event through the portal")?;

        Ok(PortalActionResult {
            action: "scroll".to_owned(),
            session: info,
            target_stream: Some(target_stream),
        })
    }

    pub async fn key_sequence(
        &mut self,
        keycodes: &[i32],
        repeat: u32,
    ) -> Result<PortalActionResult> {
        for _ in 0..repeat.max(1) {
            self.press_keycodes(keycodes).await?;
            self.release_keycodes(keycodes).await?;
        }

        Ok(PortalActionResult {
            action: "key-sequence".to_owned(),
            session: self.info(),
            target_stream: None,
        })
    }

    pub async fn hold_key_codes(
        &mut self,
        keycodes: &[i32],
        duration_ms: u64,
    ) -> Result<PortalActionResult> {
        self.press_keycodes(keycodes).await?;

        tokio::time::sleep(Duration::from_millis(duration_ms)).await;

        self.release_keycodes(keycodes).await?;

        Ok(PortalActionResult {
            action: "hold-key".to_owned(),
            session: self.info(),
            target_stream: None,
        })
    }

    pub async fn type_text(&mut self, text: &str, delay_ms: u64) -> Result<TypeActionResult> {
        for ch in text.chars() {
            let keysym = char_to_keysym(ch)?;
            self.manager
                .remote_desktop()
                .notify_keyboard_keysym(self.session.ashpd_session(), keysym, true)
                .await
                .with_context(|| {
                    format!(
                        "failed to press keysym 0x{keysym:08X} for character {:?}",
                        ch
                    )
                })?;
            // tokio::time::sleep(Duration::from_millis(TYPE_KEY_PRESS_DELAY_MS)).await;
            self.manager
                .remote_desktop()
                .notify_keyboard_keysym(self.session.ashpd_session(), keysym, false)
                .await
                .with_context(|| {
                    format!(
                        "failed to release keysym 0x{keysym:08X} for character {:?}",
                        ch
                    )
                })?;
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        Ok(TypeActionResult {
            action: "type".to_owned(),
            text: text.to_owned(),
            char_count: text.chars().count(),
        })
    }

    async fn press_keycodes(&self, keycodes: &[i32]) -> Result<()> {
        for &keycode in keycodes {
            self.manager
                .remote_desktop()
                .notify_keyboard_keycode(self.session.ashpd_session(), keycode, true)
                .await
                .with_context(|| format!("failed to press keycode {keycode} through the portal"))?;
        }

        Ok(())
    }

    async fn release_keycodes(&self, keycodes: &[i32]) -> Result<()> {
        for &keycode in keycodes.iter().rev() {
            self.manager
                .remote_desktop()
                .notify_keyboard_keycode(self.session.ashpd_session(), keycode, false)
                .await
                .with_context(|| {
                    format!("failed to release keycode {keycode} through the portal")
                })?;
        }

        Ok(())
    }

    pub async fn drag_screen_points(
        &mut self,
        from_screen: &ScreenInfo,
        from_x: i32,
        from_y: i32,
        to_screen: &ScreenInfo,
        to_x: i32,
        to_y: i32,
    ) -> Result<PortalActionResult> {
        if self.held_buttons.contains(&272) {
            bail!("left mouse button is currently held by the session");
        }

        let info = self.info();
        let from_stream = match_stream_to_screen(&info.streams, from_screen)?;
        let to_stream = match_stream_to_screen(&info.streams, to_screen)?;
        let (from_local_x, from_local_y) =
            local_stream_point(from_screen, &from_stream, from_x, from_y)?;
        let (to_local_x, to_local_y) = local_stream_point(to_screen, &to_stream, to_x, to_y)?;

        self.manager
            .remote_desktop()
            .notify_pointer_motion_absolute(
                self.session.ashpd_session(),
                from_stream.stream.node_id,
                from_local_x,
                from_local_y,
            )
            .await
            .context("failed to move pointer to drag start")?;

        self.manager
            .remote_desktop()
            .notify_pointer_button(self.session.ashpd_session(), 272, true)
            .await
            .context("failed to press left button for drag")?;

        let drag_result = async {
            self.manager
                .remote_desktop()
                .notify_pointer_motion_absolute(
                    self.session.ashpd_session(),
                    to_stream.stream.node_id,
                    to_local_x,
                    to_local_y,
                )
                .await
                .context("failed to move pointer to drag end")
        }
        .await;

        let release_result = self
            .manager
            .remote_desktop()
            .notify_pointer_button(self.session.ashpd_session(), 272, false)
            .await
            .context("failed to release left button after drag");

        drag_result?;
        release_result?;

        Ok(PortalActionResult {
            action: "drag".to_owned(),
            session: info,
            target_stream: Some(to_stream),
        })
    }

    pub async fn capture_screen_frame(&mut self, screen: &ScreenInfo) -> Result<CapturedFrame> {
        let info = self.info();
        let target_stream = match_stream_to_screen(&info.streams, screen)?;
        {
            let pipewire = self.ensure_pipewire()?;
            ensure_pipewire_stream(pipewire, &target_stream)?;
        }
        self.drain_capture_backlog();

        if let Some(frame) = self
            .latest_frames
            .get(&target_stream.stream.node_id)
            .cloned()
        {
            return Ok(CapturedFrame { frame });
        }

        let frame = {
            let pipewire = self.ensure_pipewire()?;
            recv_frame_for_stream(
                &pipewire.manager,
                target_stream.stream.node_id,
                Duration::from_secs(10),
            )?
        };
        self.latest_frames
            .insert(target_stream.stream.node_id, frame.clone());

        Ok(CapturedFrame { frame })
    }

    fn ensure_pipewire(&mut self) -> Result<&mut PersistentPipeWire> {
        if self.pipewire.is_none() {
            let fd = dup_fd(self.session.pipewire_fd().as_raw_fd())?;
            self.pipewire = Some(PersistentPipeWire {
                manager: PipeWireThreadManager::new(fd.into_raw_fd())?,
                active_streams: HashSet::new(),
            });
        }

        Ok(self
            .pipewire
            .as_mut()
            .expect("pipewire manager just initialized"))
    }
}

async fn start_session() -> Result<(PortalManager, PortalSessionHandle, Option<String>)> {
    let token_store = TokenStore::new()?;
    let saved_token = token_store.load().unwrap_or(None);
    // register_host_app("io.claude".parse()?).await?;

    match try_start_session(saved_token.clone(), true).await {
        Ok((manager, session, restore_token)) => {
            if let Some(ref token) = restore_token {
                token_store.save(token).ok();
            }
            Ok((manager, session, restore_token))
        }
        Err(error) if is_persistence_rejection(&error) => {
            let (manager, session, restore_token) = try_start_session(None, false)
                .await
                .context("failed to create session after persistence fallback")?;

            if let Some(ref token) = restore_token {
                token_store.save(token).ok();
            }

            Ok((manager, session, restore_token))
        }
        Err(error) => Err(error),
    }
}

async fn try_start_session(
    restore_token: Option<String>,
    with_persistence: bool,
) -> Result<(PortalManager, PortalSessionHandle, Option<String>)> {
    let app_id = AppID::try_from(PORTAL_APP_ID).context("invalid portal app id")?;
    if let Err(error) = register_host_app(app_id).await {
        let error_text = error.to_string();
        if error_text.contains("Connection already associated with an application ID") {
            eprintln!(
                "[kwin-portal-bridge] portal connection already has an application ID for {}; continuing",
                PORTAL_APP_ID
            );
        } else {
            return Err(error).context("failed to register host app for portal session");
        }
    }

    let manager = PortalManager::new(default_config(restore_token, with_persistence)).await?;
    let session_id = generate_session_id()?;
    let (session, restore_token) = manager
        .create_session(session_id, None)
        .await
        .context("failed to create combined RemoteDesktop/ScreenCast portal session")?;

    Ok((manager, session, restore_token))
}

fn default_config(restore_token: Option<String>, with_persistence: bool) -> PortalConfig {
    PortalConfig {
        cursor_mode: CursorMode::Embedded,
        restore_token,
        persist_mode: if with_persistence {
            PersistMode::ExplicitlyRevoked
        } else {
            PersistMode::DoNot
        },
        ..Default::default()
    }
}

fn generate_session_id() -> Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis();

    Ok(format!("kwin-portal-bridge-{timestamp}"))
}

fn session_info(session: &PortalSessionHandle, restore_token: Option<String>) -> PortalSessionInfo {
    PortalSessionInfo {
        session_id: session.session_id().to_owned(),
        pipewire_fd: session.pipewire_fd().as_raw_fd(),
        restore_token,
        remote_desktop_session: session.remote_desktop_session().map(ToOwned::to_owned),
        streams: session
            .streams()
            .iter()
            .map(|stream| PortalStream {
                node_id: stream.node_id,
                source_type: format!("{:?}", stream.source_type),
                position: [stream.position.0, stream.position.1],
                size: [
                    i32::try_from(stream.size.0).unwrap_or(i32::MAX),
                    i32::try_from(stream.size.1).unwrap_or(i32::MAX),
                ],
            })
            .collect(),
    }
}

fn to_pipewire_stream(stream: &StreamSelection) -> PwStreamInfo {
    PwStreamInfo {
        node_id: stream.stream.node_id,
        position: (stream.stream.position[0], stream.stream.position[1]),
        size: (
            u32::try_from(stream.stream.size[0]).unwrap_or_default(),
            u32::try_from(stream.stream.size[1]).unwrap_or_default(),
        ),
        source_type: match stream.stream.source_type.as_str() {
            "Window" => PwSourceType::Window,
            "Virtual" => PwSourceType::Virtual,
            _ => PwSourceType::Monitor,
        },
    }
}

fn read_one_pipewire_frame(fd: OwnedFd, stream_info: StreamInfo) -> Result<VideoFrame> {
    let manager = PipeWireThreadManager::new(fd.as_raw_fd())?;
    std::mem::forget(fd);
    let stream = StreamSelection {
        stream: PortalStream {
            node_id: stream_info.node_id,
            source_type: format!("{:?}", stream_info.source_type),
            position: [stream_info.position.0, stream_info.position.1],
            size: [
                i32::try_from(stream_info.size.0).unwrap_or(i32::MAX),
                i32::try_from(stream_info.size.1).unwrap_or(i32::MAX),
            ],
        },
    };
    let mut pipewire = PersistentPipeWire {
        manager,
        active_streams: HashSet::new(),
    };
    ensure_pipewire_stream(&mut pipewire, &stream)?;
    let frame = recv_frame_for_stream(
        &pipewire.manager,
        stream.stream.node_id,
        Duration::from_secs(10),
    )?;

    pipewire.manager.shutdown()?;
    Ok(frame)
}

fn char_to_keysym(ch: char) -> Result<i32> {
    let codepoint = u32::from(ch);
    let keysym = match ch {
        '\u{8}' => 0xFF08,
        '\t' => 0xFF09,
        '\n' | '\r' => 0xFF0D,
        '\u{1B}' => 0xFF1B,
        '\u{7F}' => 0xFFFF,
        _ if (0x20..=0x7E).contains(&codepoint) || (0xA0..=0xFF).contains(&codepoint) => codepoint,
        _ if codepoint <= 0x10_FFFF => 0x0100_0000 | codepoint,
        _ => bail!("character {:?} is outside the supported Unicode range", ch),
    };

    i32::try_from(keysym).context("keysym did not fit into i32")
}

fn ensure_pipewire_stream(
    pipewire: &mut PersistentPipeWire,
    stream: &StreamSelection,
) -> Result<()> {
    if pipewire.active_streams.contains(&stream.stream.node_id) {
        return Ok(());
    }

    let (response_tx, response_rx) = std_mpsc::sync_channel(1);
    let config = PwStreamConfig {
        name: format!("monitor-{}", stream.stream.node_id),
        width: u32::try_from(stream.stream.size[0]).unwrap_or_default(),
        height: u32::try_from(stream.stream.size[1]).unwrap_or_default(),
        framerate: 60,
        use_dmabuf: false,
        buffer_count: 3,
        preferred_format: Some(PixelFormat::BGRx),
        dmabuf_passthrough: false,
    };

    pipewire
        .manager
        .send_command(PipeWireThreadCommand::CreateStream {
            stream_id: stream.stream.node_id,
            node_id: stream.stream.node_id,
            config,
            response_tx,
        })?;

    response_rx
        .recv()
        .context("PipeWire create-stream response channel closed")?
        .context("PipeWire rejected stream creation")?;

    pipewire.active_streams.insert(stream.stream.node_id);
    Ok(())
}

fn recv_frame_for_stream(
    manager: &PipeWireThreadManager,
    stream_id: u32,
    timeout: Duration,
) -> Result<VideoFrame> {
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        if let Some(frame) = manager.recv_frame_timeout(remaining.min(Duration::from_millis(500)))
            && frame.frame_id == u64::from(stream_id)
        {
            return Ok(frame);
        }
    }

    let states = manager
        .drain_state_events()
        .into_iter()
        .map(|event| format!("stream {} -> {:?}", event.stream_id, event.state))
        .collect::<Vec<_>>();

    let states_suffix = if states.is_empty() {
        "no state events observed".to_owned()
    } else {
        format!("state events: {}", states.join(", "))
    };

    bail!("timed out waiting for PipeWire frame on stream {stream_id} ({states_suffix})");
}

fn drain_pending_frames(
    manager: &PipeWireThreadManager,
    latest_frames: &mut HashMap<u32, VideoFrame>,
) -> usize {
    let mut drained = 0;
    while let Some(frame) = manager.recv_frame_timeout(Duration::from_millis(0)) {
        let stream_id = u32::try_from(frame.frame_id).unwrap_or_default();
        latest_frames.insert(stream_id, frame);
        drained += 1;
    }
    drained
}

fn dup_fd(raw_fd: i32) -> Result<OwnedFd> {
    let duplicated = unsafe { libc::dup(raw_fd) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to duplicate portal PipeWire file descriptor");
    }

    let owned = unsafe { OwnedFd::from_raw_fd(duplicated) };
    Ok(owned)
}

fn local_stream_point(
    screen: &ScreenInfo,
    target_stream: &StreamSelection,
    x: i32,
    y: i32,
) -> Result<(f64, f64)> {
    if !point_in_screen(screen, x, y) {
        bail!("point {x},{y} is outside display `{}` bounds", screen.id);
    }

    let local_x = x - screen.geometry.x;
    let local_y = y - screen.geometry.y;
    let logical_w = screen.geometry.width.max(1) as f64;
    let logical_h = screen.geometry.height.max(1) as f64;
    let stream_w = target_stream.stream.size[0].max(1) as f64;
    let stream_h = target_stream.stream.size[1].max(1) as f64;

    Ok((
        ((local_x as f64) / logical_w) * stream_w,
        ((local_y as f64) / logical_h) * stream_h,
    ))
}

fn is_persistence_rejection(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("cannot persist") || message.contains("InvalidArgument")
}

fn match_stream_to_screen(
    streams: &[PortalStream],
    screen: &ScreenInfo,
) -> Result<StreamSelection> {
    let logical_w = screen.geometry.width;
    let logical_h = screen.geometry.height;
    let scale = screen.scale.unwrap_or(1.0);
    let physical_w = ((logical_w as f64) * scale).round() as i32;
    let physical_h = ((logical_h as f64) * scale).round() as i32;

    let exact = streams.iter().find(|stream| {
        stream.position[0] == screen.geometry.x
            && stream.position[1] == screen.geometry.y
            && (stream.size[0] == logical_w || stream.size[0] == physical_w)
            && (stream.size[1] == logical_h || stream.size[1] == physical_h)
    });

    let fallback = streams.iter().find(|stream| {
        stream.position[0] == screen.geometry.x && stream.position[1] == screen.geometry.y
    });

    let chosen = exact
        .or(fallback)
        .ok_or_else(|| anyhow::anyhow!("no portal stream matched screen `{}`", screen.id))?;

    Ok(StreamSelection {
        stream: chosen.clone(),
    })
}

pub fn point_in_screen(screen: &ScreenInfo, x: i32, y: i32) -> bool {
    x >= screen.geometry.x
        && x < screen.geometry.x.saturating_add(screen.geometry.width)
        && y >= screen.geometry.y
        && y < screen.geometry.y.saturating_add(screen.geometry.height)
}
