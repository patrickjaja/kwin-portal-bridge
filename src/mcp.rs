use std::collections::HashSet;

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

use crate::capture::{png_base64_from_frame, resolve_screen};
use crate::daemon::start_session_daemon;
use crate::desktop_apps::{AliasIndex, DesktopAppService};
use crate::executor::ExecutorBackend;
use crate::kwin::KWinBackend;
use crate::model::{
    AppRef, ClipboardReadResult, ClipboardWriteResult, CursorPosition, InstalledDesktopApp,
    KeyboardActionResult, OpenAppResult, PointerActionResult, ScreenInfo, TypeActionResult,
    WindowInfo,
};
use crate::portal::PortalBackend;
use crate::util::rects_intersect;

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopLayout {
    pub screens: Vec<ScreenInfo>,
    pub windows: Vec<WindowInfo>,
    pub frontmost_app: Option<AppRef>,
    pub cursor: CursorPosition,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FindWindowsRequest {
    #[schemars(
        description = "Match exact normalized bundle id, desktop file id, WM class, or resource name"
    )]
    pub bundle_id: Option<String>,
    #[schemars(
        description = "Match windows whose title contains this substring, case-insensitive"
    )]
    pub title_contains: Option<String>,
    #[schemars(description = "Match only windows on this screen id/output")]
    pub screen_id: Option<String>,
    #[schemars(description = "Filter by active state")]
    pub is_active: Option<bool>,
    #[schemars(description = "Filter by visible state")]
    pub is_visible: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchApplicationRequest {
    #[schemars(
        description = "Application target: bundle id, desktop id, display name, or desktop entry path"
    )]
    pub target: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActivateWindowRequest {
    #[schemars(description = "KWin window id to activate")]
    pub window_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotScreenRequest {
    #[schemars(description = "KWin screen id/output to capture")]
    pub screen_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppUnderPointRequest {
    #[schemars(description = "Global logical X coordinate")]
    pub x: i32,
    #[schemars(description = "Global logical Y coordinate")]
    pub y: i32,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveMouseRequest {
    #[schemars(description = "Global logical X coordinate")]
    pub x: i32,
    #[schemars(description = "Global logical Y coordinate")]
    pub y: i32,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClickRequest {
    #[schemars(description = "Global logical X coordinate")]
    pub x: i32,
    #[schemars(description = "Global logical Y coordinate")]
    pub y: i32,
    #[schemars(description = "Mouse button: left, right, middle, back, or forward")]
    pub button: Option<String>,
    #[schemars(description = "Number of clicks, defaults to 1")]
    pub count: Option<u32>,
    #[schemars(description = "Keyboard modifiers such as ctrl, shift, alt, or meta")]
    pub modifiers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScrollRequest {
    #[schemars(description = "Global logical X coordinate")]
    pub x: i32,
    #[schemars(description = "Global logical Y coordinate")]
    pub y: i32,
    #[schemars(description = "Horizontal scroll delta")]
    pub dx: f64,
    #[schemars(description = "Vertical scroll delta")]
    pub dy: f64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PressKeysRequest {
    #[schemars(description = "Key sequence such as ctrl+c or alt+tab")]
    pub keys: String,
    #[schemars(description = "Optional repeat count, defaults to 1")]
    pub repeat: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TypeTextRequest {
    #[schemars(description = "Text to type through the portal session")]
    pub text: String,
    #[schemars(description = "Delay in ms between characters (default 12)")]
    #[serde(default)]
    pub delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WriteClipboardRequest {
    #[schemars(description = "Text to write to the local clipboard")]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FindWindowsResult {
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstalledAppsResult {
    pub apps: Vec<InstalledDesktopApp>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActivateWindowResult {
    pub window_id: String,
    pub activated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotImageMetadata {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub display_id: String,
    pub origin_x: i32,
    pub origin_y: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotScreenMetadata {
    pub image: ScreenshotImageMetadata,
    pub screen: ScreenInfo,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppResult {
    pub app: Option<AppRef>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunningAppsResult {
    pub apps: Vec<AppRef>,
}

#[derive(Debug, Clone)]
pub struct McpServer {
    // tool_router: ToolRouter<Self>,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            // tool_router: Self::tool_router(),
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        name = "get_desktop_layout",
        description = "Return the current desktop model, including screens, windows, frontmost app, and cursor position."
    )]
    async fn get_desktop_layout(&self) -> Result<Json<DesktopLayout>, McpError> {
        let kwin = KWinBackend::new();
        let executor = ExecutorBackend::new().map_err(internal_error)?;

        let screens = kwin.list_screens().map_err(internal_error)?;
        let windows = kwin.list_windows().map_err(internal_error)?;
        let cursor = kwin.cursor_position().map_err(internal_error)?;
        let frontmost_app = executor.frontmost_app(&kwin).map_err(internal_error)?;

        Ok(Json(DesktopLayout {
            screens,
            windows,
            frontmost_app,
            cursor,
        }))
    }

    #[tool(
        name = "screenshot_screen",
        description = "Capture a single screen and return the image plus screen and window metadata."
    )]
    async fn screenshot_screen(
        &self,
        Parameters(request): Parameters<ScreenshotScreenRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kwin = KWinBackend::new();
        let portal = PortalBackend::new();
        let screens = kwin.list_screens().map_err(internal_error)?;
        let screen = resolve_screen(&screens, Some(&request.screen_id))
            .map_err(internal_error)?
            .clone();
        let captured = portal
            .capture_raw_frame_for_screen(&screen)
            .await
            .map_err(internal_error)?;
        let windows = kwin
            .list_windows()
            .map_err(internal_error)?
            .into_iter()
            .filter(|window| window_matches_screen(window, &screen))
            .collect();
        let png_base64 = png_base64_from_frame(&captured.frame).map_err(internal_error)?;
        let metadata = ScreenshotScreenMetadata {
            image: ScreenshotImageMetadata {
                mime_type: "image/png".to_owned(),
                width: captured.frame.width,
                height: captured.frame.height,
                display_width: screen.geometry.width as u32,
                display_height: screen.geometry.height as u32,
                display_id: screen.id.clone(),
                origin_x: screen.geometry.x,
                origin_y: screen.geometry.y,
            },
            screen,
            windows,
        };
        let structured_content =
            serde_json::to_value(metadata).map_err(|error| internal_error(error.into()))?;
        let mut result = CallToolResult::success(vec![Content::image(png_base64, "image/png")]);
        result.structured_content = Some(structured_content);

        Ok(result)
    }

    #[tool(
        name = "get_frontmost_app",
        description = "Return the frontmost non-bridge application."
    )]
    async fn get_frontmost_app(&self) -> Result<Json<AppResult>, McpError> {
        let kwin = KWinBackend::new();
        let executor = ExecutorBackend::new().map_err(internal_error)?;
        let app = executor.frontmost_app(&kwin).map_err(internal_error)?;
        Ok(Json(AppResult { app }))
    }

    #[tool(
        name = "app_under_point",
        description = "Return the topmost non-bridge application at a global logical point."
    )]
    async fn app_under_point(
        &self,
        Parameters(request): Parameters<AppUnderPointRequest>,
    ) -> Result<Json<AppResult>, McpError> {
        let kwin = KWinBackend::new();
        let executor = ExecutorBackend::new().map_err(internal_error)?;
        let app = executor
            .app_under_point(request.x, request.y, &kwin)
            .map_err(internal_error)?;
        Ok(Json(AppResult { app }))
    }

    #[tool(
        name = "list_installed_apps",
        description = "Return installed launchable desktop applications."
    )]
    async fn list_installed_apps(&self) -> Result<Json<InstalledAppsResult>, McpError> {
        let desktop_apps = DesktopAppService::new();
        let apps = desktop_apps.list_installed_apps().map_err(internal_error)?;
        Ok(Json(InstalledAppsResult { apps }))
    }

    #[tool(
        name = "list_running_apps",
        description = "Return running applications inferred from visible non-bridge windows."
    )]
    async fn list_running_apps(&self) -> Result<Json<RunningAppsResult>, McpError> {
        let kwin = KWinBackend::new();
        let desktop_apps = DesktopAppService::new();
        let idx = desktop_apps.alias_index().map_err(internal_error)?;
        let mut seen = HashSet::new();
        let mut apps = Vec::new();

        for window in kwin.list_windows().map_err(internal_error)? {
            if is_bridge_window(&window)
                || is_shell_window(&window)
                || !is_window_visible_for_hit_test(&window)
            {
                continue;
            }

            let app = app_ref_for_window(&window, &idx);
            if seen.insert(app.bundle_id.clone()) {
                apps.push(app);
            }
        }

        apps.sort_by(|left, right| left.display_name.cmp(&right.display_name));
        Ok(Json(RunningAppsResult { apps }))
    }

    #[tool(
        name = "find_windows",
        description = "Return windows matching a simple filter over bundle id, title, screen, and visibility."
    )]
    async fn find_windows(
        &self,
        Parameters(request): Parameters<FindWindowsRequest>,
    ) -> Result<Json<FindWindowsResult>, McpError> {
        let kwin = KWinBackend::new();
        let windows = kwin.list_windows().map_err(internal_error)?;
        let desktop_apps = DesktopAppService::new();
        let idx = desktop_apps.alias_index().map_err(internal_error)?;

        let bundle_id = request
            .bundle_id
            .as_ref()
            .map(|value| value.trim().to_ascii_lowercase());
        let title_contains = request
            .title_contains
            .as_ref()
            .map(|value| value.trim().to_ascii_lowercase());
        let screen_id = request.screen_id.as_ref().map(|value| value.trim());

        let matched = windows
            .into_iter()
            .filter(|window| {
                if let Some(bundle_id) = bundle_id.as_ref()
                    && !window_matches_bundle_id(window, bundle_id, &idx)
                {
                    return false;
                }

                if let Some(title_contains) = title_contains.as_ref()
                    && !window.title.to_ascii_lowercase().contains(title_contains)
                {
                    return false;
                }

                if let Some(screen_id) = screen_id
                    && window.output.as_deref() != Some(screen_id)
                {
                    return false;
                }

                if let Some(is_active) = request.is_active
                    && window.is_active != is_active
                {
                    return false;
                }

                if let Some(is_visible) = request.is_visible
                    && window.is_visible.unwrap_or(true) != is_visible
                {
                    return false;
                }

                true
            })
            .collect();

        Ok(Json(FindWindowsResult { windows: matched }))
    }

    #[tool(
        name = "launch_application",
        description = "Launch an installed desktop application by bundle id, desktop id, display name, or path."
    )]
    async fn launch_application(
        &self,
        Parameters(request): Parameters<LaunchApplicationRequest>,
    ) -> Result<Json<OpenAppResult>, McpError> {
        let desktop_apps = DesktopAppService::new();
        let result = desktop_apps
            .open_app(&request.target)
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "activate_window",
        description = "Activate a window by KWin window id."
    )]
    async fn activate_window(
        &self,
        Parameters(request): Parameters<ActivateWindowRequest>,
    ) -> Result<Json<ActivateWindowResult>, McpError> {
        let kwin = KWinBackend::new();
        kwin.activate_window(&request.window_id)
            .map_err(internal_error)?;

        Ok(Json(ActivateWindowResult {
            window_id: request.window_id,
            activated: true,
        }))
    }

    #[tool(
        name = "move_mouse",
        description = "Move the pointer to a global logical point."
    )]
    async fn move_mouse(
        &self,
        Parameters(request): Parameters<MoveMouseRequest>,
    ) -> Result<Json<PointerActionResult>, McpError> {
        ensure_portal_session().await.map_err(internal_error)?;

        let kwin = KWinBackend::new();
        let executor = ExecutorBackend::new().map_err(internal_error)?;
        let portal = PortalBackend::new();
        let result = executor
            .move_pointer(request.x, request.y, &portal, &kwin)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(name = "click", description = "Click at a global logical point.")]
    async fn click(
        &self,
        Parameters(request): Parameters<ClickRequest>,
    ) -> Result<Json<PointerActionResult>, McpError> {
        ensure_portal_session().await.map_err(internal_error)?;

        let kwin = KWinBackend::new();
        let executor = ExecutorBackend::new().map_err(internal_error)?;
        let portal = PortalBackend::new();
        let button = request.button.as_deref().unwrap_or("left");
        let count = request.count.unwrap_or(1);
        let modifiers = request.modifiers.unwrap_or_default();
        let result = executor
            .click_raw(
                request.x, request.y, button, count, &modifiers, &portal, &kwin,
            )
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(name = "scroll", description = "Scroll at a global logical point.")]
    async fn scroll(
        &self,
        Parameters(request): Parameters<ScrollRequest>,
    ) -> Result<Json<PointerActionResult>, McpError> {
        ensure_portal_session().await.map_err(internal_error)?;

        let kwin = KWinBackend::new();
        let executor = ExecutorBackend::new().map_err(internal_error)?;
        let portal = PortalBackend::new();
        let result = executor
            .scroll_raw(request.x, request.y, request.dx, request.dy, &portal, &kwin)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "press_keys",
        description = "Send a key sequence such as ctrl+c or alt+tab."
    )]
    async fn press_keys(
        &self,
        Parameters(request): Parameters<PressKeysRequest>,
    ) -> Result<Json<KeyboardActionResult>, McpError> {
        ensure_portal_session().await.map_err(internal_error)?;

        let executor = ExecutorBackend::new().map_err(internal_error)?;
        let portal = PortalBackend::new();
        let result = executor
            .key_sequence(&request.keys, request.repeat, &portal)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "type_text",
        description = "Type text through the active portal session."
    )]
    async fn type_text(
        &self,
        Parameters(request): Parameters<TypeTextRequest>,
    ) -> Result<Json<TypeActionResult>, McpError> {
        ensure_portal_session().await.map_err(internal_error)?;

        let executor = ExecutorBackend::new().map_err(internal_error)?;
        let portal = PortalBackend::new();
        let result = executor
            .type_text(&request.text, request.delay_ms.unwrap_or(12), &portal)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "read_clipboard",
        description = "Read text from the local clipboard."
    )]
    async fn read_clipboard(&self) -> Result<Json<ClipboardReadResult>, McpError> {
        ensure_portal_session().await.map_err(internal_error)?;

        let portal = PortalBackend::new();
        let result = portal.read_clipboard().await.map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "write_clipboard",
        description = "Write text to the local clipboard."
    )]
    async fn write_clipboard(
        &self,
        Parameters(request): Parameters<WriteClipboardRequest>,
    ) -> Result<Json<ClipboardWriteResult>, McpError> {
        ensure_portal_session().await.map_err(internal_error)?;

        let portal = PortalBackend::new();
        let result = portal
            .write_clipboard(&request.text)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "KWin desktop control primitives for local trusted use on KDE Plasma Wayland.",
        )
    }
}

pub async fn run_mcp() -> Result<()> {
    let server = McpServer::new();
    let service = server.serve(stdio()).await?;
    let _ = service.waiting().await?;
    Ok(())
}

fn internal_error(error: anyhow::Error) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

fn window_matches_bundle_id(window: &WindowInfo, expected: &str, idx: &AliasIndex) -> bool {
    // MCP's find_windows lowercases inputs, so do the same on the window
    // side; bundle ids out of the alias index are already lowercase.
    window
        .bundle_id(idx)
        .map(|id| id == expected)
        .unwrap_or(false)
}

fn app_ref_for_window(window: &WindowInfo, idx: &AliasIndex) -> AppRef {
    AppRef {
        bundle_id: window.bundle_id(idx).unwrap_or_else(|| window.id.clone()),
        display_name: window.display_name(idx),
    }
}

fn is_bridge_window(window: &WindowInfo) -> bool {
    const BRIDGE_BUNDLE_ID: &str = env!("CARGO_PKG_NAME");
    [
        window.desktop_file_name.as_deref(),
        window.resource_class.as_deref(),
        window.resource_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(|value| {
        let trimmed = value.trim();
        trimmed
            .strip_suffix(".desktop")
            .unwrap_or(trimmed)
            .to_ascii_lowercase()
    })
    .any(|value| value == BRIDGE_BUNDLE_ID)
}

fn is_shell_window(window: &WindowInfo) -> bool {
    window.is_dock.unwrap_or(false) || window.is_desktop.unwrap_or(false)
}

fn is_window_visible_for_hit_test(window: &WindowInfo) -> bool {
    if window.is_minimized.unwrap_or(false) {
        return false;
    }

    window.is_visible.unwrap_or(true)
}

fn window_matches_screen(window: &WindowInfo, screen: &ScreenInfo) -> bool {
    if let Some(output) = &window.output
        && output == &screen.id
    {
        return true;
    }

    rects_intersect(
        window.geometry.x,
        window.geometry.y,
        window.geometry.width,
        window.geometry.height,
        screen.geometry.x,
        screen.geometry.y,
        screen.geometry.width,
        screen.geometry.height,
    )
}

async fn ensure_portal_session() -> Result<()> {
    let portal = PortalBackend::new();

    match portal.create_session().await {
        Ok(_) => return Ok(()),
        Err(error) if !is_no_active_session_error(&error) => return Err(error),
        Err(_) => {}
    }

    match start_session_daemon().await {
        Ok(_) => {}
        Err(error) if is_session_already_active_error(&error) => {}
        Err(error) => return Err(error).context("failed to start portal session daemon for MCP"),
    }

    portal
        .create_session()
        .await
        .context("portal session did not become ready after startup")?;
    Ok(())
}

fn is_no_active_session_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("no active portal session"))
}

fn is_session_already_active_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .to_string()
            .contains("portal session is already active")
    })
}
