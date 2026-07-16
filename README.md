# kwin-portal-bridge

A Rust bridge that lets Anthropic's computer-use tooling actually *work* on
KDE Plasma 6.6+ under Wayland. Paired with a fork of `claude-desktop-bin`,
this gets you near feature parity with the Windows and macOS computer-use
experience — clicks, keystrokes, screenshots, window management, the whole
dance — without X11 fallbacks and without pretending Wayland doesn't exist.

## Status: experimental

This is a from-scratch reimplementation of the platform glue that Claude's
computer-use executor normally relies on. It talks to Plasma through KWin
scripts and the XDG desktop portal (RemoteDesktop + ScreenCast via
PipeWire).

Things to keep in mind before pointing it at anything important:

- **Policy enforcement is looser than upstream.** The stock Windows/macOS
  executors enforce a number of safety and allowlist policies at points
  this bridge can't always observe. Most of them are reimplemented here,
  but some are approximated and a few aren't enforced at all. If you
  depend on upstream's exact guarantees, don't assume they hold here.
- **The standalone MCP mode (`kwin-portal-bridge mcp`) is a proof of
  concept.** It exposes the bridge directly over stdio MCP without the
  desktop host wrapping it. It is missing a lot of features that the full
  executor integration has, and it performs **no security checks at all**
  — no allowlisting, no prompts, no hidden-window enforcement. Treat it
  as a toy for local experimentation. Do not point an untrusted model at
  it.

## What it does (the bragging part)

- **Real Wayland input via the RemoteDesktop portal.** Pointer move,
  click (with modifiers), drag, scroll, keyboard press/release, sustained
  key holds, and executor-style key sequences like `ctrl+shift+t`.
- **Real typing**, not synthetic key chords — text is sent as keysyms
  with configurable per-character delay, so IMEs and autocomplete
  behave sensibly.
- **Screenshots and region zoom** through the ScreenCast portal and
  PipeWire.
- **KWin-native window introspection**: enumerate screens and windows,
  query the frontmost app, find the topmost app under a point, set
  geometry, toggle keep-above, activate windows.
- **`excludeFromCapture` enforcement.** Before a screenshot or action,
  disallowed apps are hidden from capture via a `prepare-for-action`
  transaction and restored immediately after, so partial failures
  don't leak other windows' contents.
- **Long-lived portal session daemon.** One portal consent prompt per
  tool-use lock instead of one per action. The session is held open by
  a background daemon and torn down cleanly on exit.
- **Clipboard integration** via `wl-clipboard-rs`, gated on the session
  lock.
- **Desktop application integration**: enumerate installed `.desktop`
  entries, resolve icons to data URLs, launch apps by bundle id /
  desktop id / name / path (via `kioclient` for proper KDE activation).
- **Teach overlays.** An `iced` + `layer-shell` overlay for manual
  teaching bubbles, plus a session-edge overlay that indicates an
  active computer-use session without obscuring content.
- **Standalone MCP server** mode for wiring the bridge directly into
  any MCP-capable client (see caveats above).

## Requirements

- KDE Plasma 6.6 or newer, running on Wayland.
- `xdg-desktop-portal-kde` with RemoteDesktop and ScreenCast support.
- PipeWire.
- A Rust toolchain matching `edition = "2024"` (recent stable).

## Build

```sh
cargo build --release
```

The binary lands at `target/release/kwin-portal-bridge`.

## Usage

The full executor integration expects this binary to be invoked by a
fork of `claude-desktop-bin` that routes computer-use calls to it.
That's the supported path.

For quick local tinkering:

```sh
kwin-portal-bridge screens         # enumerate displays
kwin-portal-bridge windows         # enumerate windows
kwin-portal-bridge screenshot      # capture the current screen
kwin-portal-bridge mcp             # run as a standalone MCP server (PoC!)
```

`kwin-portal-bridge --help` lists every subcommand.

## License

TBD.
