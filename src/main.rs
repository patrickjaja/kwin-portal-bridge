mod capture;
mod cli;
mod daemon;
mod desktop_apps;
mod exclude_state;
mod executor;
mod json;
mod kwin;
#[cfg(feature = "mcp")]
mod mcp;
mod model;
mod portal;
mod session_overlay;
mod teach_overlay;
mod token_store;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::capture::CaptureBackend;
use crate::cli::{Cli, Command};
use crate::daemon::{
    open_session_daemon, prepare_session_socket, serve_open_session, serve_session_daemon,
    start_session_daemon, stop_session_daemon,
};
use crate::desktop_apps::DesktopAppService;
use crate::executor::ExecutorBackend;
use crate::json::print_json;
use crate::kwin::KWinBackend;
#[cfg(feature = "mcp")]
use crate::mcp::run_mcp;
use crate::model::Rect;
use crate::portal::PortalBackend;
use crate::session_overlay::run as run_session_overlay;
use crate::teach_overlay::{
    TeachStepPayload, hide as hide_teach_overlay, preview as preview_teach_overlay,
    serve as serve_teach_overlay, set_display as set_teach_display,
    set_working as set_teach_working, show_step as show_teach_step, wait_event as wait_teach_event,
};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let cli = Cli::parse();
    let kwin = KWinBackend::new;
    let capture = CaptureBackend::new;
    let desktop_apps = DesktopAppService::new;
    let executor = ExecutorBackend::new;
    let portal = PortalBackend::new;

    match cli.command {
        #[cfg(feature = "mcp")]
        Command::Mcp => {
            run_mcp().await?;
        }
        Command::SessionStart { foreground } => {
            if foreground {
                let socket = prepare_session_socket().await?;
                let (listener, session, overlay, teach_overlay) =
                    open_session_daemon(&socket).await?;
                print_json(&session.info())?;
                serve_open_session(socket, listener, session, overlay, teach_overlay).await?;
            } else {
                print_json(&start_session_daemon().await?)?;
            }
        }
        Command::SessionEnd => {
            stop_session_daemon().await?;
            print_json(&serde_json::json!({ "ended": true }))?;
        }
        Command::Screens => {
            print_json(&kwin().list_screens()?)?;
        }
        Command::Windows => {
            print_json(&kwin().list_windows()?)?;
        }
        Command::CursorPosition => {
            print_json(&kwin().cursor_position()?)?;
        }
        Command::SetWindowGeometry {
            window,
            x,
            y,
            width,
            height,
        } => {
            print_json(&kwin().set_window_geometry(
                &window,
                &Rect {
                    x,
                    y,
                    width,
                    height,
                },
            )?)?;
        }
        Command::SetWindowKeepAbove { window, value } => {
            print_json(&kwin().set_window_keep_above(&window, value)?)?;
        }
        Command::ActivateWindow { window } => {
            kwin().activate_window(&window)?;
            print_json(&serde_json::json!({ "activated": window }))?;
        }
        Command::PreviewHideSet {
            allowed_bundle_ids,
            host_bundle_id,
            display,
        } => {
            let kwin = kwin();
            print_json(&executor()?.preview_hide_set(
                &allowed_bundle_ids,
                &host_bundle_id,
                display.as_deref(),
                &kwin,
            )?)?;
        }
        Command::ListInstalledApps => {
            print_json(&desktop_apps().list_installed_apps()?)?;
        }
        Command::GetAppIcon { target } => {
            print_json(&desktop_apps().get_app_icon(&target)?)?;
        }
        Command::OpenApp { app } => {
            print_json(&desktop_apps().open_app(&app)?)?;
        }
        Command::FrontmostApp => {
            let kwin = kwin();
            print_json(&executor()?.frontmost_app(&kwin)?)?;
        }
        Command::AppUnderPoint { x, y } => {
            let kwin = kwin();
            print_json(&executor()?.app_under_point(x, y, &kwin)?)?;
        }
        Command::PointerMove { x, y } => {
            let kwin = kwin();
            print_json(&executor()?.move_pointer(x, y, &portal(), &kwin).await?)?;
        }
        Command::PointerClick {
            modifiers,
            x,
            y,
            button,
            count,
        } => {
            let kwin = kwin();
            print_json(
                &executor()?
                    .click_raw(x, y, &button, count, &modifiers, &portal(), &kwin)
                    .await?,
            )?;
        }
        Command::PointerScroll { x, y, dx, dy } => {
            let kwin = kwin();
            print_json(
                &executor()?
                    .scroll_raw(x, y, dx, dy, &portal(), &kwin)
                    .await?,
            )?;
        }
        Command::PointerDrag {
            from_x,
            from_y,
            to_x,
            to_y,
        } => {
            let kwin = kwin();
            print_json(
                &executor()?
                    .drag_raw(from_x, from_y, to_x, to_y, &portal(), &kwin)
                    .await?,
            )?;
        }
        Command::KeySequence { keys, repeat } => {
            print_json(&executor()?.key_sequence(&keys, repeat, &portal()).await?)?;
        }
        Command::Type { text, delay_ms } => {
            print_json(&executor()?.type_text(&text, delay_ms, &portal()).await?)?;
        }
        Command::HoldKey { keys, duration_ms } => {
            print_json(&executor()?.hold_keys(&keys, duration_ms, &portal()).await?)?;
        }
        Command::ReadClipboard => {
            print_json(&portal().read_clipboard().await?)?;
        }
        Command::WriteClipboard { text } => {
            print_json(&portal().write_clipboard(&text).await?)?;
        }
        Command::LeftMouseDown => {
            print_json(&portal().left_mouse_down().await?)?;
        }
        Command::LeftMouseUp => {
            print_json(&portal().left_mouse_up().await?)?;
        }
        Command::PrepareForAction {
            allowed_bundle_ids,
            host_bundle_id,
            display,
        } => {
            let kwin = kwin();
            print_json(&executor()?.prepare_for_action(
                &allowed_bundle_ids,
                &host_bundle_id,
                display.as_deref(),
                &kwin,
            )?)?;
        }
        Command::RestorePrepareState => {
            let kwin = kwin();
            print_json(&executor()?.restore_prepare_state(&kwin)?)?;
        }
        Command::Screenshot { display } => {
            let capture = capture();
            let kwin = kwin();
            print_json(
                &capture
                    .capture_still_frame(display.as_deref(), &portal(), &kwin)
                    .await?,
            )?;
        }
        Command::Zoom {
            display,
            x,
            y,
            w,
            h,
        } => {
            let capture = capture();
            let kwin = kwin();
            print_json(
                &capture
                    .capture_zoom(display.as_deref(), x, y, w, h, &portal(), &kwin)
                    .await?,
            )?;
        }
        Command::ServeSession { socket } => {
            serve_session_daemon(std::path::PathBuf::from(socket)).await?;
        }
        Command::SessionOverlay { output } => {
            run_session_overlay(output.as_deref())?;
        }
        Command::SetOverlayDisplay { display } => {
            print_json(&portal().set_overlay_display(display.as_deref()).await?)?;
        }
        Command::ServeTeachOverlay { socket } => {
            serve_teach_overlay(std::path::PathBuf::from(socket))?;
        }
        Command::TeachStep { payload, display } => {
            let payload: TeachStepPayload =
                serde_json::from_str(&payload).context("failed to decode teach step payload")?;
            print_json(&show_teach_step(payload, display)?)?;
        }
        Command::TeachWorking => {
            print_json(&set_teach_working()?)?;
        }
        Command::TeachHide => {
            print_json(&hide_teach_overlay()?)?;
        }
        Command::TeachDisplay { display } => {
            print_json(&set_teach_display(display)?)?;
        }
        Command::TeachWaitEvent => {
            print_json(&wait_teach_event()?)?;
        }
        Command::TeachOverlayPreview {
            payload,
            display,
            working,
            auto_exit_ms,
        } => {
            let payload: TeachStepPayload =
                serde_json::from_str(&payload).context("failed to decode teach preview payload")?;
            // Run the blocking iced app on a plain thread: on exit it drops
            // its internal tokio runtime, which panics inside this
            // #[tokio::main] context.
            std::thread::spawn(move || {
                preview_teach_overlay(payload, display, working, auto_exit_ms)
            })
            .join()
            .map_err(|_| anyhow::anyhow!("teach overlay preview panicked"))??;
        }
    }

    Ok(())
}
