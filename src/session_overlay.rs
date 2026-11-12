use std::f32::consts::{PI, TAU};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};
use iced::gradient;
use iced::widget::{Space, container, row, stack, text};
use iced::{Alignment, Color, Element, Length, Radians, Subscription, Task, Theme, border, window};
use iced_layershell::application;
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer};
use iced_layershell::settings::{LayerShellSettings, Settings, StartMode};
use iced_layershell::to_layer_message;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const GLOW_PERIOD_SECONDS: f32 = 1.0;
const DOT_PERIOD_SECONDS: f32 = 1.8;
const BADGE_MARGIN: f32 = 40.0;
const EDGE_FADE_STOP: f32 = 0.08;
const BADGE_FADE_START_SECONDS: f32 = 5.0;
const BADGE_FADE_DURATION_SECONDS: f32 = 0.8;

const GLOW_BANDS: [(f32, f32); 3] = [(0.16, 0.22), (0.09, 0.15), (0.03, 0.07)];

/// Set to `0` for `session-start` (the daemon it spawns inherits it) to leave
/// the iced session glow out: the caller draws its own indicator through the
/// `overlay` subcommand instead. `set-overlay-display` stays a successful
/// no-op then. Any other value, or unset: the glow runs as always.
pub const SESSION_OVERLAY_ENV: &str = "KWIN_PORTAL_BRIDGE_SESSION_OVERLAY";

/// Whether the daemon draws the iced session glow, from the value of
/// [`SESSION_OVERLAY_ENV`]. Only an exact `0` turns it off.
pub fn session_overlay_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value != Some(std::ffi::OsStr::new("0"))
}

pub struct SessionOverlayProcess {
    /// `None` when the glow is opted out ([`SESSION_OVERLAY_ENV`]).
    child: Option<Child>,
    output: Option<String>,
}

impl SessionOverlayProcess {
    /// Spawn the glow, or a disabled handle when [`SESSION_OVERLAY_ENV`] is
    /// `0`: it starts nothing, and `set_output` / `shutdown` are no-ops.
    pub fn spawn(output: Option<&str>) -> Result<Self> {
        if !session_overlay_enabled(std::env::var_os(SESSION_OVERLAY_ENV).as_deref()) {
            eprintln!("[kwin-portal-bridge] session glow off ({SESSION_OVERLAY_ENV}=0)");
            return Ok(Self {
                child: None,
                output: output.map(ToOwned::to_owned),
            });
        }
        let current_exe =
            std::env::current_exe().context("failed to resolve current executable")?;
        let mut command = Command::new(current_exe);
        command
            .arg("session-overlay")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(output) = output {
            command.arg("--output").arg(output);
        }

        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command
            .spawn()
            .context("failed to spawn session activity overlay")?;

        Ok(Self {
            child: Some(child),
            output: output.map(ToOwned::to_owned),
        })
    }

    pub fn shutdown(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }

        child.kill().ok();
        child.wait().ok();
    }

    pub fn set_output(&mut self, output: Option<&str>) -> Result<()> {
        let next_output = output.map(ToOwned::to_owned);
        if self.output == next_output {
            return Ok(());
        }
        if self.child.is_none() {
            // Opted out: nothing to move, the same empty answer as always.
            self.output = next_output;
            return Ok(());
        }

        // Spawn the replacement before killing the old overlay: the reverse
        // order leaves `self` holding a dead child but the old `output` value
        // when spawn fails, so a later retry with that value would no-op and
        // the consent indicator would stay gone for the rest of the session.
        let replacement = Self::spawn(output)?;
        self.shutdown();
        *self = replacement;
        Ok(())
    }
}

pub fn run(output: Option<&str>) -> Result<()> {
    let start_mode = match output {
        Some(output) => StartMode::TargetScreen(output.to_owned()),
        None => StartMode::Active,
    };

    application(EdgeGlow::new, namespace, update, view)
        .subscription(subscription)
        .style(app_style)
        .antialiasing(true)
        .settings(Settings {
            layer_settings: LayerShellSettings {
                size: Some((0, 0)),
                anchor: Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right,
                layer: Layer::Overlay,
                // -1 = ignore other surfaces' exclusive zones: with 0 the
                // compositor shrinks this surface to fit around panels, so
                // the glow stops at the plasma bar instead of framing the
                // whole screen.
                exclusive_zone: -1,
                keyboard_interactivity: KeyboardInteractivity::None,
                start_mode,
                events_transparent: true,
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
        .context("failed to run session activity overlay")?;

    Ok(())
}

#[derive(Debug, Clone)]
struct EdgeGlow {
    started_at: Option<Instant>,
    glow_wave: f32,
    badge_alpha: f32,
    dot_alpha: f32,
    dot_size: f32,
}

#[derive(Debug, Clone, Copy)]
enum Edge {
    Top,
    Right,
    Bottom,
    Left,
}

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    Tick(Instant),
}

impl EdgeGlow {
    fn new() -> Self {
        Self {
            started_at: None,
            glow_wave: 0.0,
            badge_alpha: 1.0,
            dot_alpha: 1.0,
            dot_size: 8.0,
        }
    }
}

fn edge_gradient(edge: Edge, alpha: f32) -> gradient::Linear {
    let hot = warm_glow(alpha);
    let clear = warm_glow(0.0);

    match edge {
        Edge::Top => gradient::Linear::new(Radians(PI * 0.5))
            .add_stop(0.0, hot)
            .add_stop(EDGE_FADE_STOP, clear),
        Edge::Bottom => gradient::Linear::new(Radians(PI * 0.5))
            .add_stop(1.0 - EDGE_FADE_STOP, clear)
            .add_stop(1.0, hot),
        Edge::Left => gradient::Linear::new(Radians(0.0))
            .add_stop(0.0, hot)
            .add_stop(EDGE_FADE_STOP, clear),
        Edge::Right => gradient::Linear::new(Radians(0.0))
            .add_stop(1.0 - EDGE_FADE_STOP, clear)
            .add_stop(1.0, hot),
    }
}

fn namespace() -> String {
    String::from("Claude Active Glow")
}

fn subscription(_: &EdgeGlow) -> Subscription<Message> {
    window::frames().map(Message::Tick)
}

fn update(glow: &mut EdgeGlow, message: Message) -> Task<Message> {
    match message {
        Message::Tick(now) => {
            let started_at = *glow.started_at.get_or_insert(now);
            let elapsed = now.duration_since(started_at).as_secs_f32();
            let glow_phase = (elapsed / GLOW_PERIOD_SECONDS) * TAU;
            let dot_phase = (elapsed / DOT_PERIOD_SECONDS) * TAU;

            glow.glow_wave = ((glow_phase.sin() + 1.0) * 0.5).clamp(0.0, 1.0);
            glow.badge_alpha = badge_alpha(elapsed);
            let dot_wave = ((dot_phase.sin() + 1.0) * 0.5).clamp(0.0, 1.0);
            glow.dot_alpha = 0.5 + dot_wave * 0.5;
            glow.dot_size = 5.6 + dot_wave * 2.4;

            Task::none()
        }
        _ => Task::none(),
    }
}

fn view(glow: &EdgeGlow) -> Element<'_, Message> {
    let mut layers: Vec<Element<'_, Message>> =
        vec![Space::new().width(Length::Fill).height(Length::Fill).into()];

    for (base_alpha, peak_alpha) in GLOW_BANDS {
        let pulse_alpha = (peak_alpha - base_alpha).max(0.0) * glow.glow_wave;

        for edge in [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
            layers.push(fullscreen_gradient(edge_gradient(edge, base_alpha)));

            if pulse_alpha > 0.0 {
                layers.push(fullscreen_gradient(edge_gradient(edge, pulse_alpha)));
            }
        }
    }

    if glow.badge_alpha > 0.0 {
        layers.push(badge_layer(glow));
    }

    stack(layers)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn fullscreen_gradient<'a>(gradient: gradient::Linear) -> Element<'a, Message> {
    container(Space::new().width(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| container::Style::from(gradient))
        .into()
}

fn badge_layer(glow: &EdgeGlow) -> Element<'_, Message> {
    let badge_alpha = glow.badge_alpha;
    let badge = container(
        row![
            dot(glow.dot_size, glow.dot_alpha * badge_alpha),
            text("Claude is using your computer")
                .size(20)
                .color(warm_glow(badge_alpha)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([8.0, 16.0])
    .style(move |_| container::Style {
        background: Some(Color::from_rgba(1.0, 0.968, 0.949, 0.95 * badge_alpha).into()),
        border: border::rounded(999.0).width(1.0).color(Color::from_rgba(
            0.851,
            0.467,
            0.341,
            0.2 * badge_alpha,
        )),
        ..container::Style::default()
    });

    container(badge)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(BADGE_MARGIN)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

fn dot(size: f32, alpha: f32) -> Element<'static, Message> {
    container(Space::new().width(size).height(size))
        .style(move |_| container::Style {
            background: Some(warm_glow(alpha).into()),
            border: border::rounded(999.0),
            ..container::Style::default()
        })
        .into()
}

fn warm_glow(alpha: f32) -> Color {
    Color::from_rgba(0.851, 0.467, 0.341, alpha.clamp(0.0, 1.0))
}

fn badge_alpha(elapsed: f32) -> f32 {
    if elapsed <= BADGE_FADE_START_SECONDS {
        return 1.0;
    }

    let fade_progress = (elapsed - BADGE_FADE_START_SECONDS) / BADGE_FADE_DURATION_SECONDS;

    (1.0 - fade_progress).clamp(0.0, 1.0)
}

fn app_style(_: &EdgeGlow, theme: &Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme.palette().text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn only_an_exact_zero_opts_out_of_the_session_glow() {
        assert!(!session_overlay_enabled(Some(OsStr::new("0"))));
        for on in [
            None,
            Some(""),
            Some("1"),
            Some("00"),
            Some(" 0"),
            Some("0 "),
            Some("false"),
            Some("off"),
        ] {
            assert!(session_overlay_enabled(on.map(OsStr::new)), "{on:?}");
        }
    }

    #[test]
    fn an_opted_out_handle_moves_and_shuts_down_as_a_no_op() {
        let mut overlay = SessionOverlayProcess {
            child: None,
            output: None,
        };
        overlay.set_output(Some("DP-1")).unwrap();
        assert_eq!(overlay.output.as_deref(), Some("DP-1"));
        overlay.set_output(None).unwrap();
        assert!(overlay.output.is_none() && overlay.child.is_none());
        overlay.shutdown();
    }
}
