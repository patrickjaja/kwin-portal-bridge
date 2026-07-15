# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 2026-07-15

### Fixed

- **`prepare-for-action` no longer fails when an allowed plasmashell surface
  refuses to focus.** With `plasmashell` on the allowlist, KDE spawns non-normal,
  non-dialog surfaces (OSD/notification/desktop containment) that KWin silently
  declines to activate. `activate_window` would poll for the requested window,
  never see it become active, and abort the whole prepare step with
  `KWin activated <A>, but <B> was requested (after 10 attempts)` - which then
  cascaded into `session daemon did not become ready in time` and a failed
  screenshot. Activation is now gated on an `is_activatable_window` check (skips
  shell surfaces and windows that report neither normal-window nor dialog) and is
  best-effort: a window KWin refuses to focus is logged and skipped instead of
  failing the request. (Reported in mosi0815/kwin-portal-bridge#1.)

- **KWin script `Variable 'DBUS_DESTINATION' is used before its declaration`
  error.** The generated KWin script emitted the header functions (which close
  over `DBUS_DESTINATION`) before the `const` declarations, which KWin's
  QJSEngine rejects under its Temporal Dead Zone rules. The bridge constants are
  now declared ahead of the header, and the four window-control scripts no longer
  redundantly re-embed the header (`render_script` already prepends it).
  (Reported in mosi0815/kwin-portal-bridge#1.)

## 2026-06-26

### Fixed

- **Window activation token race in `activate_window`.** KWin applies
  `workspace.activeWindow = target` asynchronously, so the immediate
  verification read could still observe the previously-active window and bail
  with `KWin activated <A>, but <B> was requested`. In practice this surfaced as
  the first Computer Use screenshot right after `request_access` failing on
  KDE/Wayland (the "hide-before-action" / `prepare-for-action` step), succeeding
  on retry. `activate_window` now polls the active window up to 10 times with a
  60 ms settle delay until the requested window (or one of its transient
  children) becomes active, instead of giving up on the first mismatch. The
  common case where activation has already landed still passes on the first read
  with no added latency.
  (Reported in patrickjaja/claude-desktop-bin#159.)
