# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
