use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::capture::{CaptureBackend, resolve_screen};
use crate::desktop_apps::{AliasIndex, DesktopAppService};
use crate::exclude_state::ExcludeStateStore;
use crate::kwin::KWinBackend;
use crate::model::{
    AppRef, DragActionResult, KeyboardActionResult, PointerActionResult, PrepareActionResult,
    RaiseWindowAtPointResult, ResolvePrepareCaptureResult, ScreenInfo, ScreenshotResult,
    TypeActionResult, WindowInfo,
};
use crate::portal::{PortalBackend, point_in_screen};
use crate::util;

pub struct ExecutorBackend {
    state: ExcludeStateStore,
    desktop_apps: DesktopAppService,
}

impl ExecutorBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            state: ExcludeStateStore::new()?,
            desktop_apps: DesktopAppService::new(),
        })
    }

    fn alias_index(&self) -> Result<Arc<AliasIndex>> {
        self.desktop_apps.alias_index()
    }

    pub fn preview_hide_set(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        display: Option<&str>,
        kwin: &KWinBackend,
    ) -> Result<Vec<AppRef>> {
        let idx = self.alias_index()?;
        let screens = kwin.list_screens()?;
        let screen = resolve_optional_screen(&screens, display)?;
        let windows = kwin.list_windows()?;
        let candidates =
            select_hide_candidates(&windows, screen, allowed_bundle_ids, host_bundle_id, &idx);
        Ok(to_app_refs(&candidates, &idx))
    }

    pub fn frontmost_app(&self, kwin: &KWinBackend) -> Result<Option<AppRef>> {
        let idx = self.alias_index()?;
        let windows = kwin.list_windows()?;
        Ok(frontmost_window_ignoring_bridge(&windows)
            .map(|window| app_ref_for_window(window, &idx)))
    }

    pub fn app_under_point(&self, x: i32, y: i32, kwin: &KWinBackend) -> Result<Option<AppRef>> {
        let idx = self.alias_index()?;
        let windows = kwin.list_windows()?;
        Ok(top_window_at_point_ignoring_bridge(&windows, x, y)
            .map(|window| app_ref_for_window(window, &idx)))
    }

    pub fn raise_allowed_window_at_point(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        x: i32,
        y: i32,
        kwin: &KWinBackend,
    ) -> Result<RaiseWindowAtPointResult> {
        let idx = self.alias_index()?;
        self.raise_allowed_window_at_point_inner(
            allowed_bundle_ids,
            host_bundle_id,
            x,
            y,
            kwin,
            &idx,
        )
    }

    fn raise_allowed_window_at_point_inner(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        x: i32,
        y: i32,
        kwin: &KWinBackend,
        idx: &AliasIndex,
    ) -> Result<RaiseWindowAtPointResult> {
        let windows = kwin.list_windows()?;
        let Some(topmost_window) = top_window_at_point(&windows, x, y) else {
            return Ok(RaiseWindowAtPointResult {
                topmost: None,
                raised: None,
                blocked_by: None,
            });
        };

        let topmost = app_ref_for_window(topmost_window, idx);
        if is_shell_window(topmost_window)
            || is_window_allowed(topmost_window, allowed_bundle_ids, host_bundle_id, idx)
        {
            return Ok(RaiseWindowAtPointResult {
                topmost: Some(topmost),
                raised: None,
                blocked_by: None,
            });
        }

        let target = windows_at_point_in_z_order(&windows, x, y)
            .into_iter()
            .rev()
            .skip(1)
            .find(|window| {
                !is_shell_window(window)
                    && is_window_allowed(window, allowed_bundle_ids, host_bundle_id, idx)
            });

        let Some(target) = target else {
            return Ok(RaiseWindowAtPointResult {
                topmost: Some(topmost.clone()),
                raised: None,
                blocked_by: Some(topmost),
            });
        };

        kwin.activate_window(&target.id)?;

        Ok(RaiseWindowAtPointResult {
            topmost: Some(topmost),
            raised: Some(app_ref_for_window(target, idx)),
            blocked_by: None,
        })
    }

    pub async fn click(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        x: i32,
        y: i32,
        button: &str,
        count: u32,
        modifiers: &[String],
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<PointerActionResult> {
        let screens = kwin.list_screens()?;
        let screen = screen_at_point(&screens, x, y)?;
        let raise =
            self.raise_allowed_window_at_point(allowed_bundle_ids, host_bundle_id, x, y, kwin)?;

        if let Some(blocked_by) = raise.blocked_by {
            return Ok(PointerActionResult {
                action: "click-blocked".to_owned(),
                x,
                y,
                raised: raise.raised,
                blocked_by: Some(blocked_by),
            });
        }

        let keycodes = modifiers
            .iter()
            .map(|name| key_name_to_key_code(name))
            .collect::<Result<Vec<_>>>()?;

        portal
            .click_screen_point(
                screen,
                x,
                y,
                button_name_to_evdev(button)?,
                count,
                &keycodes,
            )
            .await?;

        Ok(PointerActionResult {
            action: "click".to_owned(),
            x,
            y,
            raised: raise.raised,
            blocked_by: None,
        })
    }

    pub async fn move_pointer(
        &self,
        x: i32,
        y: i32,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<PointerActionResult> {
        let screens = kwin.list_screens()?;
        let screen = screen_at_point(&screens, x, y)?;
        portal.move_pointer_screen_point(screen, x, y).await?;

        Ok(PointerActionResult {
            action: "mouse-move".to_owned(),
            x,
            y,
            raised: None,
            blocked_by: None,
        })
    }

    pub async fn click_raw(
        &self,
        x: i32,
        y: i32,
        button: &str,
        count: u32,
        modifiers: &[String],
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<PointerActionResult> {
        let screens = kwin.list_screens()?;
        let screen = screen_at_point(&screens, x, y)?;
        let keycodes = modifiers
            .iter()
            .map(|name| key_name_to_key_code(name))
            .collect::<Result<Vec<_>>>()?;

        portal
            .click_screen_point(
                screen,
                x,
                y,
                button_name_to_evdev(button)?,
                count,
                &keycodes,
            )
            .await?;

        Ok(PointerActionResult {
            action: "click".to_owned(),
            x,
            y,
            raised: None,
            blocked_by: None,
        })
    }

    pub async fn scroll(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<PointerActionResult> {
        let screens = kwin.list_screens()?;
        let screen = screen_at_point(&screens, x, y)?;
        let raise =
            self.raise_allowed_window_at_point(allowed_bundle_ids, host_bundle_id, x, y, kwin)?;

        if let Some(blocked_by) = raise.blocked_by {
            return Ok(PointerActionResult {
                action: "scroll-blocked".to_owned(),
                x,
                y,
                raised: raise.raised,
                blocked_by: Some(blocked_by),
            });
        }

        portal.scroll_screen_point(screen, x, y, dx, dy).await?;

        Ok(PointerActionResult {
            action: "scroll".to_owned(),
            x,
            y,
            raised: raise.raised,
            blocked_by: None,
        })
    }

    pub async fn scroll_raw(
        &self,
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<PointerActionResult> {
        let screens = kwin.list_screens()?;
        let screen = screen_at_point(&screens, x, y)?;
        portal.scroll_screen_point(screen, x, y, dx, dy).await?;

        Ok(PointerActionResult {
            action: "scroll".to_owned(),
            x,
            y,
            raised: None,
            blocked_by: None,
        })
    }

    pub async fn key_sequence(
        &self,
        key_sequence: &str,
        repeat: Option<u32>,
        portal: &PortalBackend,
    ) -> Result<KeyboardActionResult> {
        let key_names = parse_key_sequence(key_sequence)?;
        let keycodes = key_names
            .iter()
            .map(|name| key_name_to_key_code(name))
            .collect::<Result<Vec<_>>>()?;
        let repeat = repeat.unwrap_or(1).max(1);

        portal.key_sequence(&keycodes, repeat).await?;

        Ok(KeyboardActionResult {
            action: "key".to_owned(),
            keys: key_names,
            repeat: Some(repeat),
            duration_ms: None,
        })
    }

    pub async fn hold_keys(
        &self,
        key_names: &[String],
        duration_ms: u64,
        portal: &PortalBackend,
    ) -> Result<KeyboardActionResult> {
        if key_names.is_empty() {
            bail!("hold key list is empty");
        }

        let keycodes = key_names
            .iter()
            .map(|name| key_name_to_key_code(name))
            .collect::<Result<Vec<_>>>()?;

        portal.hold_key_codes(&keycodes, duration_ms).await?;

        Ok(KeyboardActionResult {
            action: "hold-key".to_owned(),
            keys: key_names.to_vec(),
            repeat: None,
            duration_ms: Some(duration_ms),
        })
    }

    pub async fn type_text(
        &self,
        text: &str,
        delay_ms: u64,
        portal: &PortalBackend,
    ) -> Result<TypeActionResult> {
        portal.type_text(text, delay_ms).await
    }

    pub async fn drag(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<DragActionResult> {
        let screens = kwin.list_screens()?;
        let from_screen = screen_at_point(&screens, from_x, from_y)?;
        let to_screen = screen_at_point(&screens, to_x, to_y)?;
        let raise = self.raise_allowed_window_at_point(
            allowed_bundle_ids,
            host_bundle_id,
            from_x,
            from_y,
            kwin,
        )?;

        if let Some(blocked_by) = raise.blocked_by {
            return Ok(DragActionResult {
                action: "drag-blocked".to_owned(),
                from_x,
                from_y,
                to_x,
                to_y,
                raised: raise.raised,
                blocked_by: Some(blocked_by),
            });
        }

        portal
            .drag_screen_points(from_screen, from_x, from_y, to_screen, to_x, to_y)
            .await?;

        Ok(DragActionResult {
            action: "drag".to_owned(),
            from_x,
            from_y,
            to_x,
            to_y,
            raised: raise.raised,
            blocked_by: None,
        })
    }

    pub async fn drag_raw(
        &self,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<DragActionResult> {
        let screens = kwin.list_screens()?;
        let from_screen = screen_at_point(&screens, from_x, from_y)?;
        let to_screen = screen_at_point(&screens, to_x, to_y)?;

        portal
            .drag_screen_points(from_screen, from_x, from_y, to_screen, to_x, to_y)
            .await?;

        Ok(DragActionResult {
            action: "drag".to_owned(),
            from_x,
            from_y,
            to_x,
            to_y,
            raised: None,
            blocked_by: None,
        })
    }

    pub fn prepare_for_action(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        display: Option<&str>,
        kwin: &KWinBackend,
    ) -> Result<PrepareActionResult> {
        self.restore_prepare_state(kwin)?;

        let idx = self.alias_index()?;
        let screens = kwin.list_screens()?;
        let screen = resolve_optional_screen(&screens, display)?;
        let windows = kwin.list_windows()?;
        let candidates =
            select_hide_candidates(&windows, screen, allowed_bundle_ids, host_bundle_id, &idx);
        let changed_window_ids = windows_to_change(&candidates, true);

        if !changed_window_ids.is_empty() {
            kwin.set_exclude_from_capture(&changed_window_ids, true)?;
            self.state.save(&changed_window_ids)?;
        } else {
            self.state.clear()?;
        }

        let activated = activate_visible_windows_in_z_order(
            &windows,
            screen,
            allowed_bundle_ids,
            host_bundle_id,
            kwin,
            &idx,
        )?;

        Ok(PrepareActionResult {
            hidden: hidden_bundle_ids(&candidates, &idx),
            activated,
        })
    }

    pub fn restore_prepare_state(&self, kwin: &KWinBackend) -> Result<PrepareActionResult> {
        let managed = self.state.load()?;
        if managed.is_empty() {
            return Ok(PrepareActionResult {
                hidden: Vec::new(),
                activated: None,
            });
        }

        let existing: HashSet<_> = kwin
            .list_windows()?
            .into_iter()
            .map(|window| window.id)
            .collect();
        let restorable: Vec<String> = managed
            .into_iter()
            .filter(|window_id| existing.contains(window_id))
            .collect();

        if !restorable.is_empty() {
            kwin.set_exclude_from_capture(&restorable, false)?;
        }
        self.state.clear()?;

        Ok(PrepareActionResult {
            hidden: Vec::new(),
            activated: None,
        })
    }

    pub async fn resolve_prepare_capture(
        &self,
        allowed_bundle_ids: &[String],
        host_bundle_id: &str,
        display: Option<&str>,
        do_hide: bool,
        capture: &CaptureBackend,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<ResolvePrepareCaptureResult> {
        let idx = self.alias_index()?;
        let screens = kwin.list_screens()?;
        let windows = kwin.list_windows()?;
        let screen = resolve_capture_screen(&screens, &windows, display, host_bundle_id, &idx)?;

        let (hidden, activated, changed_window_ids) = if do_hide {
            let candidates = select_hide_candidates(
                &windows,
                Some(screen),
                allowed_bundle_ids,
                host_bundle_id,
                &idx,
            );
            let hidden = hidden_bundle_ids(&candidates, &idx);
            let activated = active_bundle_id(&windows, &idx);
            let changed_window_ids = windows_to_change(&candidates, true);

            if !changed_window_ids.is_empty() {
                kwin.set_exclude_from_capture(&changed_window_ids, true)?;
            }

            (hidden, activated, changed_window_ids)
        } else {
            (Vec::new(), None, Vec::new())
        };

        let capture_result = capture
            .capture_still_frame(Some(&screen.id), portal, kwin)
            .await;

        if !changed_window_ids.is_empty() {
            kwin.set_exclude_from_capture(&changed_window_ids, false)?;
        }

        match capture_result {
            Ok(screenshot) => Ok(resolve_capture_success(screenshot, hidden, activated)),
            Err(error) => Ok(resolve_capture_error(screen, hidden, activated, error)),
        }
    }
}

fn resolve_optional_screen<'a>(
    screens: &'a [ScreenInfo],
    selector: Option<&str>,
) -> Result<Option<&'a ScreenInfo>> {
    match selector {
        Some(_) => Ok(Some(resolve_screen(screens, selector)?)),
        None => Ok(None),
    }
}

fn resolve_capture_screen<'a>(
    screens: &'a [ScreenInfo],
    windows: &[WindowInfo],
    selector: Option<&str>,
    host_bundle_id: &str,
    idx: &AliasIndex,
) -> Result<&'a ScreenInfo> {
    match selector {
        Some(_) => resolve_screen(screens, selector),
        None => auto_capture_screen(screens, windows, host_bundle_id, idx),
    }
}

fn auto_capture_screen<'a>(
    screens: &'a [ScreenInfo],
    windows: &[WindowInfo],
    host_bundle_id: &str,
    idx: &AliasIndex,
) -> Result<&'a ScreenInfo> {
    if screens.is_empty() {
        bail!("no screens reported by KWin");
    }

    if let Some(screen) = screen_for_host_window(screens, windows, host_bundle_id, idx) {
        return Ok(screen);
    }

    screens
        .iter()
        .find(|screen| screen.is_primary)
        .or_else(|| screens.first())
        .ok_or_else(|| anyhow::anyhow!("no screen available"))
}

fn screen_at_point(screens: &[ScreenInfo], x: i32, y: i32) -> Result<&ScreenInfo> {
    screens
        .iter()
        .find(|screen| point_in_screen(screen, x, y))
        .ok_or_else(|| anyhow::anyhow!("point {x},{y} is not inside any known display"))
}

fn resolve_capture_success(
    screenshot: ScreenshotResult,
    hidden: Vec<String>,
    activated: Option<String>,
) -> ResolvePrepareCaptureResult {
    ResolvePrepareCaptureResult {
        base64: screenshot.base64,
        width: screenshot.width,
        height: screenshot.height,
        display_width: screenshot.display_width,
        display_height: screenshot.display_height,
        display_id: screenshot.display_id,
        origin_x: screenshot.origin_x,
        origin_y: screenshot.origin_y,
        hidden,
        activated,
        capture_error: None,
    }
}

fn resolve_capture_error(
    screen: &ScreenInfo,
    hidden: Vec<String>,
    activated: Option<String>,
    error: anyhow::Error,
) -> ResolvePrepareCaptureResult {
    ResolvePrepareCaptureResult {
        base64: String::new(),
        width: 0,
        height: 0,
        display_width: screen.geometry.width.max(0) as u32,
        display_height: screen.geometry.height.max(0) as u32,
        display_id: screen.id.clone(),
        origin_x: screen.geometry.x,
        origin_y: screen.geometry.y,
        hidden,
        activated,
        capture_error: Some(format!("{error:#}")),
    }
}

fn select_hide_candidates<'a>(
    windows: &'a [WindowInfo],
    screen: Option<&ScreenInfo>,
    allowed_bundle_ids: &[String],
    host_bundle_id: &str,
    idx: &AliasIndex,
) -> Vec<&'a WindowInfo> {
    windows
        .iter()
        .filter(|window| window_matches_screen(window, screen))
        .filter(|window| !is_shell_window(window))
        .filter(|window| !is_window_allowed(window, allowed_bundle_ids, host_bundle_id, idx))
        .collect()
}

fn activate_visible_windows_in_z_order(
    windows: &[WindowInfo],
    screen: Option<&ScreenInfo>,
    allowed_bundle_ids: &[String],
    host_bundle_id: &str,
    kwin: &KWinBackend,
    idx: &AliasIndex,
) -> Result<Option<String>> {
    let mut visible_windows: Vec<_> = windows
        .iter()
        .filter(|window| window_matches_screen(window, screen))
        .filter(|window| !is_shell_window(window))
        .filter(|window| !is_bridge_window(window))
        .filter(|window| !is_host_window(window, host_bundle_id, idx))
        .filter(|window| is_window_visible_for_hit_test(window))
        .collect();

    visible_windows.sort_by_key(|window| window.stacking_order);

    let mut seen_allowed = false;
    let mut needs_activation = false;
    for window in visible_windows.iter().rev() {
        if is_window_allowed(window, allowed_bundle_ids, host_bundle_id, idx) {
            seen_allowed = true;
            continue;
        }

        if seen_allowed {
            needs_activation = true;
            break;
        }
    }

    if !needs_activation {
        return Ok(None);
    }

    let activatable: Vec<_> = visible_windows
        .into_iter()
        .filter(|window| is_window_allowed(window, allowed_bundle_ids, host_bundle_id, idx))
        .filter(|window| is_activatable_window(window))
        .collect();

    let mut activated = None;
    for window in activatable {
        // Raising allowed apps above disallowed ones is best-effort. KWin
        // silently ignores `workspace.activeWindow = target` for surfaces it
        // refuses to focus (e.g. a plasmashell containment / OSD that slipped
        // past the shell filter), which makes activate_window time out. That
        // must not abort the whole prepare-for-action step and block the
        // screenshot, so log and keep going instead of propagating the error.
        match kwin.activate_window(&window.id) {
            Ok(()) => activated = bundle_id_for_window(window, idx),
            Err(error) => {
                eprintln!(
                    "[kwin-portal-bridge] skipping non-activatable window {}: {error:#}",
                    window.id
                );
            }
        }
    }

    Ok(activated)
}

/// Whether KWin will accept `workspace.activeWindow = window`. Desktop,
/// dock, and shell surfaces are already excluded upstream; this additionally
/// drops utility/OSD/notification surfaces (plasmashell spawns several) that
/// report neither normal-window nor dialog and which KWin refuses to focus.
/// When KWin reports neither flag we assume activatable to stay conservative.
fn is_activatable_window(window: &WindowInfo) -> bool {
    if is_shell_window(window) {
        return false;
    }
    !matches!(
        (window.is_normal_window, window.is_dialog),
        (Some(false), Some(false))
    )
}

fn to_app_refs(windows: &[&WindowInfo], idx: &AliasIndex) -> Vec<AppRef> {
    let mut seen = HashSet::<String>::new();
    let mut apps = Vec::new();

    for window in windows {
        if is_bridge_window(window) {
            continue;
        }

        let Some(bundle_id) = bundle_id_for_window(window, idx) else {
            continue;
        };

        if seen.insert(bundle_id.clone()) {
            apps.push(AppRef {
                bundle_id,
                display_name: display_name_for_window(window, idx),
            });
        }
    }

    apps
}

fn windows_to_change(windows: &[&WindowInfo], value: bool) -> Vec<String> {
    windows
        .iter()
        .filter(|window| window.exclude_from_capture != value)
        .map(|window| window.id.clone())
        .collect()
}

fn hidden_bundle_ids(windows: &[&WindowInfo], idx: &AliasIndex) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut hidden = Vec::new();

    for window in windows {
        if is_bridge_window(window) {
            continue;
        }

        let Some(bundle_id) = bundle_id_for_window(window, idx) else {
            continue;
        };

        if seen.insert(bundle_id.clone()) {
            hidden.push(bundle_id);
        }
    }

    hidden
}

fn app_ref_for_window(window: &WindowInfo, idx: &AliasIndex) -> AppRef {
    AppRef {
        bundle_id: bundle_id_for_window(window, idx).unwrap_or_else(|| window.id.clone()),
        display_name: display_name_for_window(window, idx),
    }
}

fn button_name_to_evdev(button: &str) -> Result<i32> {
    match button.trim().to_ascii_lowercase().as_str() {
        "left" => Ok(272),
        "right" => Ok(273),
        "middle" => Ok(274),
        "back" => Ok(278),
        "forward" => Ok(277),
        other => bail!("unsupported pointer button `{other}`"),
    }
}

fn parse_key_sequence(key_sequence: &str) -> Result<Vec<String>> {
    let parts = key_sequence
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if parts.is_empty() {
        bail!("key sequence is empty");
    }

    Ok(parts)
}

fn key_name_to_key_code(name: &str) -> Result<i32> {
    let normalized = name.trim().to_ascii_lowercase();
    let code = match normalized.as_str() {
        "ctrl" | "control" | "leftctrl" => 29,
        "shift" | "leftshift" => 42,
        "alt" | "option" | "leftalt" => 56,
        "meta" | "super" | "command" | "cmd" | "leftmeta" => 125,
        "enter" | "return" => 28,
        "tab" => 15,
        "space" => 57,
        "backspace" => 14,
        "delete" => 111,
        "esc" | "escape" => 1,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "a" => 30,
        "b" => 48,
        "c" => 46,
        "d" => 32,
        "e" => 18,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "i" => 23,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "m" => 50,
        "n" => 49,
        "o" => 24,
        "p" => 25,
        "q" => 16,
        "r" => 19,
        "s" => 31,
        "t" => 20,
        "u" => 22,
        "v" => 47,
        "w" => 17,
        "x" => 45,
        "y" => 21,
        "z" => 44,
        "0" => 11,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        _ => bail!("unsupported key name `{name}`"),
    };

    Ok(code)
}

fn active_bundle_id(windows: &[WindowInfo], idx: &AliasIndex) -> Option<String> {
    frontmost_window_ignoring_bridge(windows).and_then(|window| bundle_id_for_window(window, idx))
}

fn frontmost_window_ignoring_bridge(windows: &[WindowInfo]) -> Option<&WindowInfo> {
    windows
        .iter()
        .filter(|window| !is_bridge_window(window))
        .filter(|window| is_window_visible_for_hit_test(window))
        .max_by_key(|window| {
            (
                if window.is_active { 1_u8 } else { 0_u8 },
                window.stacking_order,
            )
        })
}

fn screen_for_host_window<'a>(
    screens: &'a [ScreenInfo],
    windows: &[WindowInfo],
    host_bundle_id: &str,
    idx: &AliasIndex,
) -> Option<&'a ScreenInfo> {
    windows
        .iter()
        .filter(|window| is_host_window(window, host_bundle_id, idx))
        .filter(|window| !is_shell_window(window))
        .filter(|window| is_window_visible_for_hit_test(window))
        .max_by_key(|window| {
            (
                if window.is_active { 1_u8 } else { 0_u8 },
                window.stacking_order,
            )
        })
        .and_then(|window| screen_for_window(screens, window))
}

fn bundle_id_for_window(window: &WindowInfo, idx: &AliasIndex) -> Option<String> {
    window.bundle_id(idx)
}

fn display_name_for_window(window: &WindowInfo, idx: &AliasIndex) -> String {
    window.display_name(idx)
}

fn is_bridge_window(window: &WindowInfo) -> bool {
    // Bridge windows (including the teach/session overlays we spawn) are
    // tagged with our process's executable name as their X11 class. The
    // binary is usually `kwin-portal-bridge` but may be renamed at install
    // time, so consult `util::bridge_overlay_names()` for both.
    let overlay_names = util::bridge_overlay_names();
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
    .any(|value| overlay_names.iter().any(|name| name == &value))
}

fn is_shell_window(window: &WindowInfo) -> bool {
    window.is_dock.unwrap_or(false) || window.is_desktop.unwrap_or(false)
}

fn is_host_window(window: &WindowInfo, host_bundle_id: &str, idx: &AliasIndex) -> bool {
    bundle_id_for_window(window, idx)
        .map(|bundle_id| bundle_id == host_bundle_id)
        .unwrap_or(false)
}

/// Compare a window's canonical bundle id against the allowlist verbatim.
/// Inputs are treated as canonical — upstream owns alias resolution by
/// querying `list-installed-apps` and matching strings.
fn is_window_allowed(
    window: &WindowInfo,
    allowed_bundle_ids: &[String],
    host_bundle_id: &str,
    idx: &AliasIndex,
) -> bool {
    let Some(bundle_id) = bundle_id_for_window(window, idx) else {
        return false;
    };
    bundle_id == host_bundle_id || allowed_bundle_ids.iter().any(|id| id == &bundle_id)
}

fn top_window_at_point(windows: &[WindowInfo], x: i32, y: i32) -> Option<&WindowInfo> {
    windows_at_point_in_z_order(windows, x, y)
        .into_iter()
        .next_back()
}

fn top_window_at_point_ignoring_bridge(
    windows: &[WindowInfo],
    x: i32,
    y: i32,
) -> Option<&WindowInfo> {
    windows_at_point_in_z_order(windows, x, y)
        .into_iter()
        .rev()
        .find(|window| !is_bridge_window(window))
}

fn windows_at_point_in_z_order(windows: &[WindowInfo], x: i32, y: i32) -> Vec<&WindowInfo> {
    let mut hits: Vec<_> = windows
        .iter()
        .filter(|window| is_window_visible_for_hit_test(window))
        .filter(|window| rect_contains_point(&window.geometry, x, y))
        .collect();

    hits.sort_by_key(|window| window.stacking_order);
    hits
}

fn is_window_visible_for_hit_test(window: &WindowInfo) -> bool {
    if window.is_minimized.unwrap_or(false) {
        return false;
    }

    window.is_visible.unwrap_or(true)
}

fn screen_for_window<'a>(screens: &'a [ScreenInfo], window: &WindowInfo) -> Option<&'a ScreenInfo> {
    if let Some(output) = &window.output
        && let Some(screen) = screens
            .iter()
            .find(|screen| screen.id == *output || screen.name == *output)
    {
        return Some(screen);
    }

    screens
        .iter()
        .filter_map(|screen| {
            let overlap = rect_intersection_area(
                window.geometry.x,
                window.geometry.y,
                window.geometry.width,
                window.geometry.height,
                screen.geometry.x,
                screen.geometry.y,
                screen.geometry.width,
                screen.geometry.height,
            );
            (overlap > 0).then_some((screen, overlap))
        })
        .max_by_key(|(_, overlap)| *overlap)
        .map(|(screen, _)| screen)
}

fn window_matches_screen(window: &WindowInfo, screen: Option<&ScreenInfo>) -> bool {
    let Some(screen) = screen else {
        return true;
    };

    if let Some(output) = &window.output
        && output == &screen.id
    {
        return true;
    }

    util::rects_intersect(
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

fn rect_contains_point(rect: &crate::model::Rect, x: i32, y: i32) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

fn rect_intersection_area(
    ax: i32,
    ay: i32,
    aw: i32,
    ah: i32,
    bx: i32,
    by: i32,
    bw: i32,
    bh: i32,
) -> i64 {
    if aw <= 0 || ah <= 0 || bw <= 0 || bh <= 0 {
        return 0;
    }

    let left = ax.max(bx);
    let top = ay.max(by);
    let right = ax.saturating_add(aw).min(bx.saturating_add(bw));
    let bottom = ay.saturating_add(ah).min(by.saturating_add(bh));

    if right <= left || bottom <= top {
        return 0;
    }

    i64::from(right - left) * i64::from(bottom - top)
}

#[cfg(test)]
mod tests {
    use super::{
        hidden_bundle_ids, is_activatable_window, is_window_allowed, select_hide_candidates,
        to_app_refs, windows_to_change,
    };

    const BRIDGE_BUNDLE_ID: &str = env!("CARGO_PKG_NAME");
    use crate::desktop_apps::AliasIndex;
    use crate::model::{Rect, WindowInfo};

    fn test_window(id: &str, bundle_id: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_owned(),
            title: bundle_id.to_owned(),
            geometry: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            pid: None,
            desktop_file_name: Some(bundle_id.to_owned()),
            resource_class: Some(bundle_id.to_owned()),
            resource_name: Some(bundle_id.to_owned()),
            window_role: None,
            window_type: None,
            is_dock: Some(false),
            is_desktop: Some(false),
            is_visible: Some(true),
            is_minimized: Some(false),
            is_normal_window: Some(true),
            is_dialog: Some(false),
            transient: Some(false),
            transient_for: None,
            output: None,
            stacking_order: 0,
            is_active: false,
            exclude_from_capture: false,
            keep_above: Some(false),
        }
    }

    #[test]
    fn plasmashell_osd_surface_is_not_activation_target() {
        // Regression guard for the mosi0815/kwin-portal-bridge#1 failure:
        // plasmashell is on the allowlist and spawns non-normal, non-dialog
        // surfaces (OSD/notification/containment) that KWin refuses to focus.
        // Trying to activate one makes prepare-for-action time out, so such
        // surfaces must be excluded from the activation set.
        let mut osd = test_window("{osd}", "org.kde.plasmashell");
        osd.is_normal_window = Some(false);
        osd.is_dialog = Some(false);
        assert!(!is_activatable_window(&osd));

        // A regular application window stays activatable.
        let app = test_window("{firefox}", "firefox");
        assert!(is_activatable_window(&app));

        // A dialog stays activatable.
        let mut dialog = test_window("{dialog}", "firefox");
        dialog.is_normal_window = Some(false);
        dialog.is_dialog = Some(true);
        assert!(is_activatable_window(&dialog));

        // Unknown flags stay conservative (assume activatable).
        let mut unknown = test_window("{unknown}", "firefox");
        unknown.is_normal_window = None;
        unknown.is_dialog = None;
        assert!(is_activatable_window(&unknown));

        // Docks/desktop shells are never activation targets.
        let mut dock = test_window("{panel}", "org.kde.plasmashell");
        dock.is_dock = Some(true);
        assert!(!is_activatable_window(&dock));
    }

    #[test]
    fn hide_responses_skip_bridge_windows() {
        let idx = AliasIndex::default();
        let bridge = test_window("{bridge}", BRIDGE_BUNDLE_ID);
        let firefox = test_window("{firefox}", "firefox");
        let windows = vec![bridge, firefox];

        let candidates = select_hide_candidates(&windows, None, &[], "claude", &idx);

        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|window| window.id == "{bridge}"));
        assert!(candidates.iter().any(|window| window.id == "{firefox}"));
        assert_eq!(
            windows_to_change(&candidates, true),
            vec!["{bridge}".to_owned(), "{firefox}".to_owned()]
        );
        assert_eq!(hidden_bundle_ids(&candidates, &idx), vec!["firefox"]);

        let preview = to_app_refs(&candidates, &idx);
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0].bundle_id, "firefox");
    }

    #[test]
    fn allowlist_matches_canonical_window_id_verbatim() {
        // KCalc-shaped fixture: file stem `org.kde.kcalc`, StartupWMClass
        // `kcalc`. Index ensures both forms canonicalize to `kcalc`.
        let idx = AliasIndex::for_tests([
            ("org.kde.kcalc", "kcalc"),
            ("org.kde.kcalc.desktop", "kcalc"),
            ("kcalc", "kcalc"),
        ]);
        let window = test_window("{kcalc}", "org.kde.kcalc");

        // Canonical form wired through: a window reporting any KCalc alias
        // matches an allowlist that contains the canonical `kcalc`.
        let allowed = vec!["kcalc".to_owned()];
        assert!(is_window_allowed(&window, &allowed, "claude", &idx));
    }

    #[test]
    fn allowlist_rejects_non_canonical_aliases() {
        // Same fixture, but the allowlist holds the FDO id instead of the
        // canonical bundle id. Bridge does no input normalization, so this
        // does NOT match — upstream is expected to send canonical ids.
        let idx = AliasIndex::for_tests([("org.kde.kcalc", "kcalc"), ("kcalc", "kcalc")]);
        let window = test_window("{kcalc}", "kcalc");

        let allowed = vec!["org.kde.kcalc".to_owned()];
        assert!(!is_window_allowed(&window, &allowed, "claude", &idx));
    }
}
