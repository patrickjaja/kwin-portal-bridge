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
- **Long-lived portal session daemon.** One portal consent prompt the
  first time ("Allow restoring on future sessions" stays on); the restore
  token is stored under `$XDG_STATE_HOME/kwin-portal-bridge/` and every
  later session starts without a prompt. The session is held open by a
  background daemon and torn down cleanly on exit. Every wait is bounded:
  `session-start` waits up to 120 s for the consent dialog and reports a
  declined dialog at once, each request has a client timeout, and
  `session-end` reaps a daemon that does not answer (SIGTERM, then SIGKILL
  of its process group). The daemon's pid and stderr live next to its
  socket, in `$XDG_RUNTIME_DIR/kwin-portal-bridge/session.pid` and
  `daemon.log`. Persistence is
  requested through `RemoteDesktop.SelectDevices` only - the portal
  refuses it on `ScreenCast.SelectSources` for a remote-desktop session,
  which is why the token was never stored before.
- **Forgetting the screen-sharing choice.** `kwin-portal-bridge
  forget-session` ends any session daemon (same bounded reaping as
  `session-end`) and deletes the stored restore token, so the next session
  shows the consent dialog again; a plain `session-end` would replay the
  same selection. It prints one JSON object (`ok`, `ended`, `daemon`,
  `token_removed`, `token_path`), succeeds when nothing was there to remove,
  leaves other state such as the exclude list alone, and needs no running
  KDE session.
- **Noticing when the user stops sharing.** Plasma shows a screen-sharing
  indicator while the session runs, and the user can end it there. The
  daemon watches the portal session's `Session.Closed` signal; when it
  fires it writes `$XDG_RUNTIME_DIR/kwin-portal-bridge/stopped-by-user`,
  removes its socket and pidfile, restores hidden windows and exits. A
  request in flight at that moment fails with exit 5, and while the marker
  exists every input and capture command (pointer, keyboard, typing,
  screenshot, zoom, clipboard) exits 5 at once without opening a portal
  session. `screens`, `windows`, `doctor` and the other non-portal commands
  keep working; `session-start`, `session-end` and `forget-session` remove
  the marker.
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

## CLI contract v1

The bridge implements the
[Computer Use bridge CLI contract v1](https://github.com/patrickjaja/claude-desktop-extra/blob/master/docs/computer-use-bridge-contract.md)
that claude-desktop-extra uses for every bridge:

- `doctor` reports `contract: 1`, the version, whether this is a KDE Plasma
  Wayland session, whether KWin and the portal are on the session bus, the
  restore-token and socket paths, whether the session daemon answers and
  `stopped_by_user` (the user ended screen sharing). It exits 0 even with no
  session bus, and never opens a portal session.
- `session-start` prints `{"ok":true,"session":"daemon"}` (the restore token
  never reaches stdout), `session-end` prints `ok`, `ended` and `daemon`,
  `forget-session` adds `token_removed` and `token_path`.
- Errors go to stderr with nothing on stdout. Exit codes: 1 runtime error,
  2 bad usage, 3 the target monitor is not shared in the portal session,
  4 no live session (the daemon is gone), 5 the user stopped screen sharing.
  The daemon sends the error class with its reply, so 3, 4 and 5 hold for
  errors raised inside it.
- Coordinate flags take negative values (`pointer-move --x -1920 --y 0`),
  for monitors left of or above the origin.

`cargo test` runs `tests/contract.rs` against the built binary in an
isolated environment.

## `abort-watch`: Escape stops Computer Use (optional contract addition)

`kwin-portal-bridge abort-watch` is a long-running watcher that tells Claude
Desktop when the user presses Escape. The protocol, signal and parent-death
handling come from the shared
[`cu-abort-watch`](https://github.com/patrickjaja/cu-abort-watch) crate:

- stdout, one JSON object per line:
  `{"event":"ready","mechanism":"kglobalaccel","syntheticSafe":false}` once
  armed, `{"event":"abort"}` per Escape press,
  `{"event":"ack","cmd":"suspend"|"resume"}`, `{"event":"error","message":...}`.
- stdin: `suspend` / `resume`, each acknowledged.
- Exit 0 on stdin EOF, SIGTERM/SIGINT/SIGHUP or parent death (after removing
  the shortcut); 1 after an `error` line when kglobalaccel or the session bus
  goes away, a `resume` fails, or a `suspend` lasts longer than 5 s; 2 when it
  cannot arm (nothing registered).

It only needs `org.kde.kglobalaccel` on the session bus, so it works on any
KWin, X11 or Wayland, also below Plasma 6.6 where the rest of the bridge is not
used. It never starts kglobalaccel (calls are sent without D-Bus auto-start).

### What the user notices while it is armed

kglobalaccel global shortcuts take a key before almost everything else in
KWin's input chain (the global shortcut filter runs before effects,
interactive move/resize, popups and the focused application). While Computer
Use runs with the watcher armed, **every Escape press stops Computer Use and
does nothing else**: it does not close Overview or other effects, does not
cancel a window move or resize, does not close popup menus, and does not reach
any application. The lock screen and Alt+Tab are not affected. The grab is
released as soon as the watcher exits.

The watcher arms but never fires where global shortcuts are not delivered:
under a kiosk setup that restricts global shortcuts (`useAllowList`), and
while a client inhibits shortcuts (for example a remote-desktop or VM viewer
with keyboard grab). `doctor` only checks that kglobalaccel is running; it
reports `available` even when Escape is already taken by another shortcut
(then `abort-watch` refuses with exit 2).

### How it works

D-Bus `org.kde.KGlobalAccel` on `/kglobalaccel`:

- Component `claude-cu-abort` ("Claude: stop Computer Use"), action
  `_k_session:claude-cu-abort-escape`.
- Arming: if a previous watcher of this session was killed and left its
  registration (no live holder), it is removed first. Then
  `globalShortcutsByKey` for bare Escape with match types Equal (exactly
  Escape) and Shadows (every sequence that contains Escape, such as `Esc, X`
  or `Ctrl+X, Esc`). If another component owns one, the watcher refuses with
  an `error` line naming it and exit 2, and changes nothing. Otherwise
  `doRegister` and `setShortcutKeys([Escape], SetPresent | NoAutoloading)`,
  then it listens for the component's `globalShortcutPressed` signal. Before
  Plasma 6.7 kglobalaccel itself drops a key that another shortcut uses; that
  is also reported and exits 2.
- kglobalaccel cannot tell a physical key from one injected through the
  RemoteDesktop portal or ydotool, so `syntheticSafe` is false: send
  `suspend` before injecting Escape and `resume` after. `suspend` releases
  the grab (`setInactive`), so the injected Escape reaches the app. `resume`
  keeps it released for another 150 ms (`RESUME_SETTLE` in
  `src/abort_watch.rs`): portal key delivery is asynchronous, and this delay
  is what keeps a late injected Escape from being grabbed and reported. Then
  it re-grabs Escape and, before acknowledging, drains the press signals
  kglobalaccel queued before the re-grab (a ping round trip to its own
  listening connection); no `abort` is reported in between.
- A suspension is bounded: a watcher suspended for more than 5 s
  (`SUSPEND_LIMIT`) fails with exit 1, so a caller that never resumes cannot
  leave Escape released. Wrap only the Escape key event itself in
  `suspend` / `resume`, not a long `type` or `holdKey`.
- The shortcut is per session bus and shared by every Claude profile on that
  bus (holder set in `$XDG_RUNTIME_DIR/claude-cu-esc/kglobalaccel-<bus id>/`,
  so two graphical sessions of one user stay separate): the first watcher
  registers it, the last one removes the action and the component
  (`setInactive` + `cleanUp`). Each watcher reports the press itself, so one
  Escape stops Computer Use in every armed profile. A suspension releases
  Escape for all profiles while it lasts; suspensions are counted
  (`kglobalaccel-<bus id>-suspended/`), so Escape is grabbed again only when
  no profile is suspended, and a suspended watcher that died is pruned within
  about 0.5 s by the others, which grab Escape again.
- `claude-cu-esc/` must be a real directory owned by the user without
  group/other access (a looser mode is tightened to 0700; a symlink or another
  owner refuses to arm with exit 2). The holder directories are created only
  once kglobalaccel is found, and a failed arm removes them again unless
  another watcher uses them.
- A watcher killed with SIGKILL leaves the grab in kglobalaccel's memory
  until the next watcher in that session arms (it removes the leftover first,
  even when it then refuses to arm) or the session ends.

### `kglobalshortcutsrc`

The Escape binding itself is never written to disk: kglobalaccel does not
save `_k_session:` actions. kglobalaccel does save its settings 500 ms after
registration (and on any other shortcut change while armed), and that write
includes the component group without any key:

```ini
[claude-cu-abort]
_k_friendly_name=Claude: stop Computer Use
```

A normal exit removes it at once (`cleanUp` writes immediately). After a
SIGKILL, or when kglobalaccel exits while the watcher is armed, the empty
group stays in `~/.config/kglobalshortcutsrc` until the next watcher arms and
exits, or until kglobalaccel's next settings write after a restart (an empty
component is dropped then). Until then System Settings may list "Claude: stop
Computer Use" with no shortcut.

`doctor` reports `abortWatch`: available when `org.kde.kglobalaccel` is
running on the session bus and `XDG_RUNTIME_DIR` is set. It never registers
anything. `abortWatch.consumes` is `true` when available (kglobalaccel takes
Escape from applications while armed), `null` otherwise.

Tests: `tests/abort_watch.rs` drives the binary against a mock kglobalaccel
(Equal/Shadows matching as in kglobalacceld) on private `dbus-daemon`s (CI
sets `KPB_REQUIRE_DBUS_TESTS=1`). `scripts/kglobalaccel-smoke/run.sh` runs the
same flow against the real kglobalaccel daemon library on a private bus and
checks the real `kglobalshortcutsrc` (see the script header).

## License

TBD.
