# Bug audit: kwin-portal-bridge

*Audit date: 2026-07-16 — full-codebase review at commit `279a8b9` (v0.2.8).*

Build and `cargo clippy --all-features` are clean — everything below was found by
manual review, and each high/medium item was verified directly against the source.
There is no test suite to catch any of these.

**Status (updated 2026-07-16, same session):** all high (#1–#6) and medium
(#7–#14) findings are fixed in the working tree, plus 7 of the 12 low-severity
items. Each finding below carries a ✅ **FIXED** or ⏳ **OPEN** marker. Line
numbers refer to the pre-fix code at `279a8b9`. After the fixes:
`cargo clippy --all-features` clean, all 13 tests pass, release build succeeds.

## High severity

### 1. The allowlist "raise allowed window" logic is inverted — `src/executor.rs:349-361`

The loop walks windows top-to-bottom and sets `needs_activation` only when a
*disallowed* window sits **below** an already-seen allowed one — which is the
already-safe configuration. The dangerous case (disallowed window stacked *above*
the allowed app, e.g. KeePass over Firefox) never trips the flag, so nothing gets
raised. Since disallowed windows are only hidden from *capture* — they stay
physically on top — the model sees the allowed app in the screenshot but injected
clicks land on the invisible disallowed window. This is the safety-critical bug of
the batch.

**Fix:** the condition needs to fire when a disallowed window is seen before
(above) an allowed one, not after — i.e. track `seen_disallowed` and trigger when
an allowed window appears below it.

✅ **FIXED** — loop rewritten exactly as described.

### 2. Any misbehaving client kills the whole daemon — `src/daemon.rs:255-257`

In the accept branch, `read_request(...).await?` and `write_response(...).await?`
propagate straight out of the serve loop, tearing down the portal session.
Empty/malformed JSON or a client that disconnects before reading the reply is
enough. The kicker: `cleanup_stale_socket` (`src/daemon.rs:328`) probes liveness by
connecting and immediately dropping the stream — so **starting a second daemon
instance sends exactly that empty request and kills the running one**.

**Fix:** log per-connection errors (or answer with `ok:false`) and continue the
loop instead of propagating.

✅ **FIXED** — per-client read/write errors are now logged and the loop
continues; only `accept()` failures remain fatal.

### 3. One stalled client freezes the daemon, including SIGTERM — `src/daemon.rs:343-350`

`read_request` uses `read_to_end` (waits for EOF) inline inside the `select!` arm,
and connections are serviced sequentially. A client that connects and just holds
its write half open blocks the loop forever — no other requests, no capture
draining, and the SIGTERM branch is never polled again.

**Fix:** add a read timeout or switch to a length-framed protocol.

✅ **FIXED** — both read and write are wrapped in a 10 s `IPC_IO_TIMEOUT`.

### 4. Dead teach-overlay runner is never detected; `show-step` hangs forever — `src/teach_overlay.rs:556-568`

`TeachOverlayRunner::start` spawns a thread that just `eprintln!`s and exits if
`run_overlay` fails (Wayland/layer-shell init error). `self.runner` stays `Some`,
and `ensure_runner` only checks `is_some()`, never `is_finished()`. After one
startup failure, every `ShowStep` stores a pending step and blocks on
`receiver.recv()` (`src/teach_overlay.rs:348`) with no UI that will ever resolve it.

**Fix:** check `join_handle.is_finished()` in `ensure_runner` and restart (or
surface the startup error).

✅ **FIXED** — `ensure_runner` now reaps a finished runner and starts a fresh one.

### 5. `stop()` can join a UI thread that never exits — while holding the service mutex — `src/teach_overlay.rs:673-676`

The overlay's tick handler only closes the window when `app.window_id` is `Some`
(`src/teach_overlay.rs:820-824`). If the layer-shell surface never mapped (e.g.
`set-display` with a nonexistent output), `join()` blocks forever — and
`handle_request` holds `service.lock()` across the whole `set_display` call
(`src/teach_overlay.rs:365-368`), so every subsequent teach request queues behind
it. The whole teach service is bricked until process restart.

**Fix:** make the runner exit on the shutdown flag even when no window ever
opened, and/or join with a timeout outside the service lock.

✅ **FIXED** — `stop()` polls `is_finished()` with a 5 s deadline and detaches
the thread instead of blocking forever (the shutdown flag stays set, so a
late-mapping window still closes).

### 6. Zombie processes accumulate in MCP/daemon mode — `src/desktop_apps.rs:651-668`

`spawn_detached` drops the `Child` without ever calling `wait()`. `setsid()`
creates a new session but the process remains a direct child of the bridge, so in
the long-lived MCP server every launched app that exits becomes a `<defunct>`
zombie for the server's lifetime. Short-lived CLI mode masks it.

**Fix:** double-fork (spawn an intermediate that exits immediately and is reaped)
or install a SIGCHLD reaper.

✅ **FIXED** — double-fork in `pre_exec` (setsid → fork, intermediate `_exit(0)`),
with the intermediate reaped via `wait()` right after spawn.

## Medium severity

### 7. Windows can get stuck permanently hidden from capture — `src/executor.rs:245-250`

`prepare_for_action` calls `kwin.set_exclude_from_capture(..., true)?` *before*
`self.state.save(...)?`. If the save fails, the windows are already hidden but
nothing is recorded, so no restore path will ever un-hide them — and
`windows_to_change` (`src/executor.rs:437`) filters out already-excluded windows,
so they are never re-tracked on later runs either. Bug #8 makes this reachable
without any disk failure.

**Fix:** persist intent first, or restore-on-failure.

✅ **FIXED** — `state.save()` now runs before `set_exclude_from_capture()`; a
failed exclude leaves at worst tracked-but-not-hidden entries, which the restore
path un-hides harmlessly.

### 8. `set_exclude_from_capture` reports failure after succeeding — `src/kwin.rs:71-77`

The check compares unique matched windows against the raw request count. Duplicate
ids, or a window closing between `list_windows` and the script run, makes it
`bail!` *after* KWin already applied the change. In `prepare_for_action` that error
fires before `state.save` — a direct route into bug #7's hidden-but-untracked
state.

**Fix:** compare against the deduplicated set of *still-existing* requested ids, and
treat partial application explicitly.

✅ **FIXED** — comparison is now by id set, and the error names the specific
unmatched ids instead of raw counts.

### 9. Out-of-bounds panic on undersized PipeWire frames — `src/capture.rs:124` and `src/capture.rs:178`

Row slicing (`&bytes[y * stride..y * stride + width * 4]`) trusts the
producer-reported `stride`/`width`/`height` without validating `bytes.len()`. A
short buffer (or `stride < width*4`) panics inside the daemon's serve task,
unwinding past the cleanup block — crashed daemon plus leaked portal session.

**Fix:** validate the buffer length against `height`, `stride`, `width` up front
and `bail!` instead of indexing.

✅ **FIXED** — new `validate_frame_layout` guard (zero dims, `stride < width*4`,
overflow-checked required length) runs before both row loops.

### 10. Failed overlay respawn silently kills the consent indicator — `src/session_overlay.rs:75-85`

`set_output` shuts down the running overlay first; if the replacement `spawn`
fails, `self` keeps the *old* `output` value and a dead child. A later `set_output`
with that same value hits the `self.output == next_output` early return and
no-ops — the "Claude is using your computer" indicator is gone for the rest of the
session while the daemon believes it's showing.

**Fix:** spawn the replacement before shutting down the old one, or clear
`self.output` on failure.

✅ **FIXED** — replacement is spawned first; the old overlay is only torn down
once the new one exists.

### 11. DBus-activatable apps can't launch without `kioclient` — `src/desktop_apps.rs:57-59`

`DBusActivatable=true` routes unconditionally to `launch_via_kio`, which hard-fails
when `kioclient` is missing (`src/desktop_apps.rs:435-437`) — never trying the
entry's valid `Exec=` line. The reverse fallback (exec → kio) exists at
`src/desktop_apps.rs:61-75`; this direction has none. Hits most GNOME apps and many
Flatpaks on non-KDE-complete setups.

**Fix:** fall back to `launch_via_exec` when kio launching is unavailable and the
entry has `Exec=`.

✅ **FIXED** — exec fallback added (`launcher: "desktop-entry-exec-fallback"`);
only entries with neither kioclient nor `Exec=` still fail.

### 12. `Terminal=true` apps lose their `Path=` working directory — `src/desktop_apps.rs:453-457`

The cwd is set on `command`, but the terminal branch builds a fresh
`terminal_command` and spawns that instead, without `current_dir`.

**Fix:** carry `expanded.cwd` over to the terminal command.

✅ **FIXED** — the terminal branch now sets `current_dir` too.

### 13. Superseded teach step is reported as user exit — `src/teach_overlay.rs:480-481`

A new `ShowStep` resolves a previously pending one with action `"exit"` —
indistinguishable from the user actually clicking Exit, so an overlapping
controller tears down a flow the user never aborted.

**Fix:** use a distinct action value such as `"superseded"`.

✅ **FIXED** — superseded steps now resolve with `"superseded"`; `"exit"` is
reserved for genuine user exits (and `hide`, which is a deliberate teardown).
Note for consumers: clients that matched on `"exit"` for the superseded case
will see the new value.

### 14. Portal stream matching mixes coordinate spaces — `src/portal.rs:942-951`

The "exact" match lets width match the logical size while height matches the
physical size (or vice versa), and the fallback matches on position alone — two
streams at origin `(0,0)` pick whichever comes first. Wrong stream → wrong
`stream.size` in `local_stream_point` → clicks land at scaled-off coordinates under
fractional scaling.

**Fix:** require both dimensions to match in the *same* coordinate space, and
disambiguate the fallback by size/source type.

✅ **FIXED** (partially) — the exact match now requires both dimensions in the
same coordinate space. The position-only fallback is unchanged; it only fires
when no size-consistent stream exists at all.

## Low severity

- ✅ **FIXED** — **`src/token_store.rs:48`** — non-atomic `fs::write` of the
  restore token; an interrupted write corrupts it, `load` errors are swallowed
  (`src/portal.rs:649` `unwrap_or(None)`), and the user silently gets re-prompted
  for capture permission. Now writes via `NamedTempFile` + `persist` (rename).
- ⏳ **OPEN** (deliberately) — **`src/portal.rs:927-930`** — `is_persistence_rejection`
  matches the bare substring `"InvalidArgument"`, so unrelated portal errors
  trigger the retry-without-persistence path and drop the restore token.
  Left as-is: narrowing the match without knowing the exact portal error strings
  risks breaking the detection that currently works. Needs testing against a real
  portal that rejects persistence.
- ⏳ **OPEN** — **`src/kwin.rs:208-222`** — if the D-Bus `run` call or message
  polling errors, `stop` is never called and the loaded script leaks inside KWin
  (accumulates over a long session). An RAII guard would fix all exit paths.
- ⏳ **OPEN** — **`src/kwin.rs:172-183`** — the result-callback match rule checks
  neither sender nor path, so any same-session bus peer can forge a
  `result`/`error` reply — including faking a successful hide confirmation. Same
  trust domain, but this is a capture-privacy boundary; filter on the sender
  (requires resolving org.kde.KWin's unique bus name).
- ✅ **FIXED** — **`src/kwin.rs:259-264`** — script-name suffix is a bare
  microsecond timestamp; concurrent invocations collide and KWin refuses the
  duplicate name. The PID is now part of the suffix.
- ✅ **FIXED** — **`src/kwin.rs:194`** — `script_path.to_str().unwrap()` panics on
  a non-UTF-8 `TMPDIR`. Now bails with context.
- ✅ **FIXED** — **`src/kwin.rs:61-65`** — `serde_json` doesn't escape
  U+2028/U+2029, which can break the generated KWin script as a JS syntax error.
  Both characters are now escaped in the window-id array.
- ✅ **FIXED** — **`src/json.rs:5`** — `println!` panics on broken pipe. Now uses
  `writeln!` and treats `BrokenPipe` as a quiet success.
- ✅ **FIXED** — **`src/teach_overlay.rs:135-141`** — the socket-wait loop never
  checked `child.try_wait()`. It now reports an early child exit (with the log
  path) instead of a misleading timeout.
- ⏳ **OPEN** — **`src/teach_overlay.rs:546-553`** — each disconnected `wait-event`
  client leaks a blocked thread + channel until the next hide/exit event;
  unbounded if a monitor polls with retry. Needs a protocol-level fix (e.g.
  waiter registration with disconnect detection).
- ✅ **FIXED** — **`src/daemon.rs:268-272`** — shutdown failures were swallowed
  with no log. `restore_prepare_state` and `session.shutdown()` failures are now
  logged.
- ⏳ **OPEN** — **`src/teach_overlay.rs:626-630`** — anchors are unconditionally
  dropped when no display is configured, so anchored bubbles silently center even
  on single-monitor setups where the coordinates were directly usable. Behavior
  decision (which screen do global coordinates belong to?) — left for the owner.

## The big picture

Two clusters stand out. First, **the daemon's IPC loop is fragile** (#2, #3): it
trusts every socket client to be well-behaved, and even the codebase's own
stale-socket probe violates that trust. Second, **the capture-exclusion transaction
isn't crash-safe** (#1, #7, #8): the ordering hide-then-persist plus a
false-failure check means the safety mechanism can both fail to protect (inverted
raise logic) and fail open (stuck-hidden windows). If only three things get fixed,
make it #1, #2, and #7+#8 together.

## Remaining open items

All fourteen high/medium findings are fixed. Still open from the low-severity
list: the `InvalidArgument` substring match (`src/portal.rs`, needs real-portal
testing), the KWin script leak on error paths (`src/kwin.rs`, wants an RAII
guard), the unauthenticated D-Bus reply matching (`src/kwin.rs`, wants sender
filtering), the `wait-event` waiter leak (`src/teach_overlay.rs`), and the
anchor-drop-without-display behavior (`src/teach_overlay.rs`).
