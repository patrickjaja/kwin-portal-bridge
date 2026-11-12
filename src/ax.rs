//! AT-SPI subcommands (`ax-enable`, `ax-restore`, `ax-find`, `ax-action`)
//! and the `atspi` key of `doctor`, on top of the shared `cu-atspi` crate.
//!
//! These run directly in the CLI process, never through the session daemon:
//! they need only the session bus (KWin scripting for the window lookup) and
//! the accessibility bus, never a portal session, so they never show a consent
//! dialog and work whether or not a daemon is running. The stopped-by-user
//! marker does not apply to them.

use std::time::Duration;

use anyhow::Result;
use cu_atspi::cli::AxFindFlags;
use cu_atspi::{AtspiDoctor, Rect as AxRect, WindowHint};
use serde_json::{Value, json};

use crate::kwin::{AxWindow, KWinBackend};
use crate::model::Rect;

/// Budget for `doctor.atspi`, so `doctor` stays well inside the launcher's
/// 5 s kill (the crate's own deadline is 5 s).
const DOCTOR_ATSPI_BUDGET: Duration = Duration::from_secs(3);

/// `doctor.atspi`. Never fails: the crate reports its own errors in `error`;
/// the timeout fallback still carries the feature list.
pub async fn doctor_atspi() -> Value {
    let report = match tokio::time::timeout(DOCTOR_ATSPI_BUDGET, cu_atspi::doctor()).await {
        Ok(report) => report,
        Err(_) => AtspiDoctor {
            error: Some(format!(
                "accessibility probe timed out after {} s",
                DOCTOR_ATSPI_BUDGET.as_secs()
            )),
            ..AtspiDoctor::default()
        },
    };
    serde_json::to_value(report)
        .unwrap_or_else(|error| json!({ "bus": false, "error": error.to_string() }))
}

/// `ax-find`: resolve the window through KWin, then walk its AT-SPI tree.
pub async fn find(kwin: &KWinBackend, window_id: &str, flags: &AxFindFlags) -> Result<Value> {
    find_in(kwin.ax_window(window_id)?, window_id, flags).await
}

/// `ax-find` once KWin answered the lookup (`None`: no such window).
async fn find_in(window: Option<AxWindow>, window_id: &str, flags: &AxFindFlags) -> Result<Value> {
    let Some(window) = window else {
        return Ok(window_not_found(window_id));
    };
    let request = flags.request(window_hint(&window));
    Ok(serde_json::to_value(cu_atspi::find(&request).await?)?)
}

fn window_not_found(window_id: &str) -> Value {
    json!({
        "unavailable": "window_not_found",
        "hint": format!("KWin has no window with id {window_id}; list the windows again."),
    })
}

/// KWin window -> crate hint. KWin geometry is global logical px, the space
/// the crate's frame classification expects.
pub fn window_hint(window: &AxWindow) -> WindowHint {
    WindowHint {
        id: Some(window.id.clone()),
        output: non_empty(window.output.as_deref()),
        pid: window.pid.filter(|pid| *pid > 0),
        app_id: non_empty(
            window
                .desktop_file_name
                .as_deref()
                .map(|name| name.strip_suffix(".desktop").unwrap_or(name)),
        ),
        wm_class: non_empty(window.resource_class.as_deref()),
        title: non_empty(Some(window.title.as_str())),
        rect_screen_logical: Some(ax_rect(&window.frame_geometry)),
        client_rect_screen_logical: window.client_geometry.as_ref().map(ax_rect),
        // Unknown counts as native Wayland: the non-X11 classification still
        // accepts global extents that line up with the frame or client rect.
        x11: window.x11.unwrap_or(false),
    }
}

fn ax_rect(rect: &Rect) -> AxRect {
    AxRect::new(rect.x, rect.y, rect.width, rect.height)
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kwin_window(json: &str) -> AxWindow {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn kwin_window_maps_to_the_crate_hint() {
        let window = kwin_window(
            r#"{"id":"{a1}","title":"Untitled - Kate","pid":4242,
                "desktop_file_name":"org.kde.kate.desktop","resource_class":"kate",
                "output":"DP-1",
                "frame_geometry":{"x":100,"y":50,"width":800,"height":630},
                "client_geometry":{"x":100,"y":80,"width":800,"height":600},
                "x11":false}"#,
        );
        let hint = window_hint(&window);
        assert_eq!(hint.id.as_deref(), Some("{a1}"));
        assert_eq!(hint.pid, Some(4242));
        assert_eq!(hint.app_id.as_deref(), Some("org.kde.kate"));
        assert_eq!(hint.wm_class.as_deref(), Some("kate"));
        assert_eq!(hint.title.as_deref(), Some("Untitled - Kate"));
        assert_eq!(hint.output.as_deref(), Some("DP-1"));
        assert_eq!(
            hint.rect_screen_logical,
            Some(AxRect::new(100, 50, 800, 630))
        );
        assert_eq!(
            hint.client_rect_screen_logical,
            Some(AxRect::new(100, 80, 800, 600))
        );
        assert!(!hint.x11);
    }

    #[test]
    fn xwayland_window_sets_x11() {
        let window = kwin_window(
            r#"{"id":"w","title":"gimp","pid":7,
                "frame_geometry":{"x":0,"y":0,"width":10,"height":10},"x11":true}"#,
        );
        assert!(window_hint(&window).x11);
    }

    #[test]
    fn missing_or_empty_fields_stay_unset() {
        let window = kwin_window(
            r#"{"id":"w","title":"","pid":0,"desktop_file_name":"","resource_class":" ",
                "output":null,"frame_geometry":{"x":-1920,"y":0,"width":10,"height":10},
                "client_geometry":null,"x11":null}"#,
        );
        let hint = window_hint(&window);
        assert_eq!(hint.pid, None);
        assert_eq!(hint.app_id, None);
        assert_eq!(hint.wm_class, None);
        assert_eq!(hint.title, None);
        assert_eq!(hint.output, None);
        assert_eq!(hint.client_rect_screen_logical, None);
        assert_eq!(
            hint.rect_screen_logical,
            Some(AxRect::new(-1920, 0, 10, 10))
        );
        assert!(!hint.x11, "unknown client type counts as native Wayland");
    }

    #[test]
    fn find_request_carries_the_filters() {
        let window =
            kwin_window(r#"{"id":"w","frame_geometry":{"x":0,"y":0,"width":10,"height":10}}"#);
        let flags = AxFindFlags {
            role: Some("AXButton".into()),
            title_contains: Some("Save".into()),
            all: true,
            include_hidden: true,
            max_nodes: Some(50),
            max_depth: Some(4),
            ..AxFindFlags::default()
        };
        let request = flags.request(window_hint(&window));
        assert_eq!(request.role.as_deref(), Some("AXButton"));
        assert_eq!(request.title_contains.as_deref(), Some("Save"));
        assert!(request.all && request.include_hidden);
        assert_eq!((request.max_nodes, request.max_depth), (Some(50), Some(4)));
        assert_eq!(request.window.id.as_deref(), Some("w"));
    }

    #[tokio::test]
    async fn find_for_an_unknown_window_is_unavailable_without_any_bus() {
        let value = find_in(None, "{gone}", &AxFindFlags::default())
            .await
            .unwrap();
        assert_eq!(value["unavailable"], "window_not_found");
    }

    #[test]
    fn unknown_window_is_unavailable() {
        let value = window_not_found("{nope}");
        assert_eq!(value["unavailable"], "window_not_found");
        assert!(value["hint"].as_str().unwrap().contains("{nope}"));
    }
}
