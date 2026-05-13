#[cfg(feature = "mcp")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::desktop_apps::AliasIndex;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct ScreenInfo {
    pub id: String,
    pub name: String,
    pub geometry: Rect,
    pub scale: Option<f64>,
    pub refresh_millihz: Option<u32>,
    pub is_active: bool,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub geometry: Rect,
    pub pid: Option<u32>,
    pub desktop_file_name: Option<String>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub window_role: Option<String>,
    pub window_type: Option<String>,
    pub is_dock: Option<bool>,
    pub is_desktop: Option<bool>,
    pub is_visible: Option<bool>,
    pub is_minimized: Option<bool>,
    pub is_normal_window: Option<bool>,
    pub is_dialog: Option<bool>,
    pub transient: Option<bool>,
    pub transient_for: Option<Box<WindowAppRef>>,
    pub output: Option<String>,
    pub stacking_order: usize,
    pub is_active: bool,
    pub exclude_from_capture: bool,
    pub keep_above: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct WindowAppRef {
    pub id: String,
    pub desktop_file_name: Option<String>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub transient: Option<bool>,
    pub transient_for: Option<Box<WindowAppRef>>,
}

impl WindowInfo {
    pub fn bundle_id(&self, idx: &AliasIndex) -> Option<String> {
        bundle_id_from_parts(
            idx,
            Some(self.id.as_str()),
            self.desktop_file_name.as_deref(),
            self.resource_class.as_deref(),
            self.resource_name.as_deref(),
            self.transient.unwrap_or(false),
            self.transient_for.as_deref(),
        )
    }

    pub fn display_name(&self, idx: &AliasIndex) -> String {
        if !self.title.trim().is_empty() {
            return self.title.clone();
        }

        self.bundle_id(idx).unwrap_or_else(|| self.id.clone())
    }

    pub fn is_transient_for_window(&self, window_id: &str) -> bool {
        self.transient_for
            .as_deref()
            .is_some_and(|window| window.references_window(window_id))
    }
}

impl WindowAppRef {
    fn bundle_id(&self, idx: &AliasIndex) -> Option<String> {
        bundle_id_from_parts(
            idx,
            Some(self.id.as_str()),
            self.desktop_file_name.as_deref(),
            self.resource_class.as_deref(),
            self.resource_name.as_deref(),
            self.transient.unwrap_or(false),
            self.transient_for.as_deref(),
        )
    }

    fn references_window(&self, window_id: &str) -> bool {
        self.id == window_id
            || self
                .transient_for
                .as_deref()
                .is_some_and(|window| window.references_window(window_id))
    }
}

fn bundle_id_from_parts(
    idx: &AliasIndex,
    id: Option<&str>,
    desktop_file_name: Option<&str>,
    resource_class: Option<&str>,
    resource_name: Option<&str>,
    transient: bool,
    transient_for: Option<&WindowAppRef>,
) -> Option<String> {
    if transient && let Some(bundle_id) = transient_for.and_then(|parent| parent.bundle_id(idx)) {
        return Some(bundle_id);
    }

    for candidate in [desktop_file_name, resource_class, resource_name, id] {
        if let Some(value) = candidate
            && !value.trim().is_empty()
        {
            return Some(idx.canonicalize(value));
        }
    }

    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowControlResult {
    pub window_id: String,
    pub geometry: Rect,
    pub keep_above: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludeUpdate {
    pub windows: Vec<String>,
    pub value: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotResult {
    pub base64: String,
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub display_id: String,
    pub origin_x: i32,
    pub origin_y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
pub struct ScreenshotCapture {
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AppRef {
    pub bundle_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrepareActionResult {
    pub hidden: Vec<String>,
    pub activated: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RaiseWindowAtPointResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topmost: Option<AppRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised: Option<AppRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<AppRef>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PointerActionResult {
    pub action: String,
    pub x: i32,
    pub y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised: Option<AppRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<AppRef>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct KeyboardActionResult {
    pub action: String,
    pub keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TypeActionResult {
    pub action: String,
    pub text: String,
    pub char_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClipboardReadResult {
    pub action: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClipboardWriteResult {
    pub action: String,
    pub text: String,
    pub char_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DragActionResult {
    pub action: String,
    pub from_x: i32,
    pub from_y: i32,
    pub to_x: i32,
    pub to_y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised: Option<AppRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<AppRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonStateResult {
    pub action: String,
    pub button: String,
    pub is_held: bool,
    pub was_held: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvePrepareCaptureResult {
    pub base64: String,
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub display_id: String,
    pub origin_x: i32,
    pub origin_y: i32,
    pub hidden: Vec<String>,
    pub activated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct InstalledDesktopApp {
    pub bundle_id: String,
    pub display_name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OpenAppResult {
    pub opened: bool,
    pub bundle_id: String,
    pub display_name: String,
    pub path: String,
    pub launcher: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolPresence {
    pub command: String,
    pub available: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub tools: Vec<ToolPresence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalStream {
    pub node_id: u32,
    pub source_type: String,
    pub position: [i32; 2],
    pub size: [i32; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalSessionInfo {
    pub session_id: String,
    pub pipewire_fd: i32,
    pub restore_token: Option<String>,
    pub remote_desktop_session: Option<String>,
    pub streams: Vec<PortalStream>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSelection {
    pub stream: PortalStream,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalActionResult {
    pub action: String,
    pub session: PortalSessionInfo,
    pub target_stream: Option<StreamSelection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameProbeResult {
    pub session: PortalSessionInfo,
    pub target_stream: StreamSelection,
    pub frame: FrameInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameInfo {
    pub frame_id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: String,
    pub buffer_kind: String,
    pub bytes: Option<usize>,
    pub dmabuf_planes: Option<usize>,
    pub flags: u32,
    pub damage_regions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedImageResult {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub session: PortalSessionInfo,
    pub target_stream: StreamSelection,
    pub frame: lamco_pipewire::VideoFrame,
    pub frame_byte_len: usize,
}

#[cfg(test)]
mod tests {
    use super::{AliasIndex, Rect, WindowAppRef, WindowInfo};

    fn empty_index() -> AliasIndex {
        AliasIndex::default()
    }

    fn test_window(
        id: &str,
        title: &str,
        desktop_file_name: Option<&str>,
        resource_class: Option<&str>,
        transient: Option<bool>,
        transient_for: Option<WindowAppRef>,
    ) -> WindowInfo {
        WindowInfo {
            id: id.to_owned(),
            title: title.to_owned(),
            geometry: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            pid: None,
            desktop_file_name: desktop_file_name.map(str::to_owned),
            resource_class: resource_class.map(str::to_owned),
            resource_name: Some("soffice.bin".to_owned()),
            window_role: None,
            window_type: None,
            is_dock: Some(false),
            is_desktop: Some(false),
            is_visible: Some(true),
            is_minimized: Some(false),
            is_normal_window: Some(true),
            is_dialog: Some(false),
            transient,
            transient_for: transient_for.map(Box::new),
            output: None,
            stacking_order: 0,
            is_active: false,
            exclude_from_capture: false,
            keep_above: Some(false),
        }
    }

    #[test]
    fn transient_windows_resolve_bundle_id_from_parent() {
        let idx = empty_index();
        let transient_for = WindowAppRef {
            id: "{calc}".to_owned(),
            desktop_file_name: Some("libreoffice-calc".to_owned()),
            resource_class: Some("libreoffice-calc".to_owned()),
            resource_name: Some("soffice.bin".to_owned()),
            transient: Some(false),
            transient_for: None,
        };

        let dialog = test_window(
            "{dialog}",
            "Eigenschaften von \"Unbenannt 1\"",
            Some("libreoffice-startcenter"),
            Some("libreoffice-startcenter"),
            Some(true),
            Some(transient_for),
        );

        assert_eq!(dialog.bundle_id(&idx).as_deref(), Some("libreoffice-calc"));
    }

    #[test]
    fn transient_windows_fall_back_to_their_own_identifiers_when_parent_is_missing() {
        let idx = empty_index();
        let dialog = test_window(
            "{dialog}",
            "",
            Some("libreoffice-startcenter"),
            Some("libreoffice-startcenter"),
            Some(true),
            None,
        );

        assert_eq!(
            dialog.bundle_id(&idx).as_deref(),
            Some("libreoffice-startcenter")
        );
        assert_eq!(dialog.display_name(&idx), "libreoffice-startcenter");
    }

    #[test]
    fn transient_windows_report_their_parent_chain() {
        let root = WindowAppRef {
            id: "{root}".to_owned(),
            desktop_file_name: Some("libreoffice-calc".to_owned()),
            resource_class: Some("libreoffice-calc".to_owned()),
            resource_name: Some("soffice.bin".to_owned()),
            transient: Some(false),
            transient_for: None,
        };
        let parent = WindowAppRef {
            id: "{parent}".to_owned(),
            desktop_file_name: Some("libreoffice-startcenter".to_owned()),
            resource_class: Some("libreoffice-startcenter".to_owned()),
            resource_name: Some("soffice.bin".to_owned()),
            transient: Some(true),
            transient_for: Some(Box::new(root)),
        };
        let dialog = test_window(
            "{dialog}",
            "Nested transient",
            Some("libreoffice-startcenter"),
            Some("libreoffice-startcenter"),
            Some(true),
            Some(parent),
        );

        assert!(dialog.is_transient_for_window("{parent}"));
        assert!(dialog.is_transient_for_window("{root}"));
        assert!(!dialog.is_transient_for_window("{other}"));
    }

    #[test]
    fn unmapped_window_falls_back_to_lowercased_resource_class() {
        let idx = empty_index();
        let window = test_window(
            "{firefox}",
            "Firefox",
            None,
            Some("Firefox"),
            Some(false),
            None,
        );

        assert_eq!(window.bundle_id(&idx).as_deref(), Some("firefox"));
    }

    #[test]
    fn window_with_desktop_suffix_strips_it() {
        let idx = empty_index();
        let window = test_window(
            "{kcalc}",
            "KCalc",
            Some("org.kde.kcalc.desktop"),
            Some("kcalc"),
            Some(false),
            None,
        );

        // No alias index: desktop_file_name wins (first in candidate order),
        // `.desktop` stripped, lowercased -> "org.kde.kcalc".
        assert_eq!(window.bundle_id(&idx).as_deref(), Some("org.kde.kcalc"));
    }

    #[test]
    fn kde_app_canonicalizes_via_alias_index() {
        // Mirrors a real KCalc install: file stem `org.kde.kcalc`, FDO id
        // `org.kde.kcalc`, StartupWMClass `kcalc`. All three resolve to the
        // canonical bundle id `kcalc`.
        let idx = AliasIndex::for_tests([
            ("org.kde.kcalc", "kcalc"),
            ("org.kde.kcalc.desktop", "kcalc"),
            ("kcalc", "kcalc"),
        ]);

        let window = test_window(
            "{kcalc}",
            "KCalc",
            Some("org.kde.kcalc"),
            Some("kcalc"),
            Some(false),
            None,
        );

        assert_eq!(window.bundle_id(&idx).as_deref(), Some("kcalc"));
    }
}
