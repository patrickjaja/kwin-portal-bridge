//! `abort-watch` (optional part of the Computer Use bridge CLI contract): "press Escape to stop
//! Computer Use" on KDE, through kglobalaccel.
//!
//! The protocol, signals, parent-death handling and the holder-set mechanics
//! live in the shared `cu-abort-watch` crate; this module is the KDE backend.
//! It needs nothing but `org.kde.kglobalaccel` on the session bus, so it
//! works on any KWin (X11 and Wayland, Plasma 5 and 6, also below 6.6 where
//! the rest of this bridge is not used).
//!
//! Registration (D-Bus API: `org.kde.KGlobalAccel` on `/kglobalaccel`, see
//! kglobalacceld `src/kglobalacceld.cpp`):
//!
//! - action id `[component, action, componentFriendly, actionFriendly]` =
//!   [`COMPONENT`], [`ACTION`], [`COMPONENT_FRIENDLY`], [`ACTION_FRIENDLY`];
//! - refuse when `globalShortcutsByKey(Escape)` reports another component
//!   (match types Equal and Shadows: bare Escape and every sequence that
//!   contains Escape);
//! - `doRegister(as)`, then `setShortcutKeys(as, a(ai), u)` with
//!   `SetPresent | NoAutoloading`: bare Escape exactly, grabbed at once;
//! - the action name starts with `_k_session:`, so kglobalacceld never writes
//!   the key binding to `kglobalshortcutsrc`
//!   (`GlobalShortcut::isSessionShortcut`). The component group itself
//!   (`[claude-cu-abort]` with `_k_friendly_name`) IS written by any settings
//!   write while armed; disarming removes it (`cleanUp` writes at once).
//!
//! `syntheticSafe` is false: kglobalaccel cannot tell a physical key from one
//! injected through the RemoteDesktop portal (KWin fake input) or ydotool.
//! `suspend` releases the grab (`setInactive`). `resume` waits
//! [`RESUME_SETTLE`] with the grab still released (this is what lets a late
//! injected Escape reach the app instead of being reported), re-grabs
//! (`setShortcutKeys` with `SetPresent`) and then drains the press signals
//! kglobalaccel queued before the re-grab (a `Peer.Ping` round trip to the
//! listening connection), all while the crate keeps reporting muted.
//!
//! The registration is per session bus, shared by every Claude profile on
//! that bus through a holder set keyed by the bus id (first holder registers,
//! last holder removes the component). Suspensions are counted in a second
//! holder set, so one profile's `resume` does not re-grab Escape while
//! another profile still injects one; a suspension affects every profile
//! while it lasts. A watchdog bounds it: a watcher suspended longer than
//! [`SUSPEND_LIMIT`] fails (exit 1, the others re-grab), and a suspender that
//! died is pruned within [`WATCH_TICK`] and Escape grabbed again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use cu_abort_watch::{AbortSink, Backend, BoxError, Mechanism, Ready, SharedBinding, binding_dir};
use dbus::Message;
use dbus::blocking::{BlockingSender, Connection};
use dbus::channel::MatchingReceiver;
use dbus::message::MatchRule;

/// kglobalaccel component of the abort shortcut (System Settings group).
pub const COMPONENT: &str = "claude-cu-abort";
pub const COMPONENT_FRIENDLY: &str = "Claude: stop Computer Use";
/// `_k_session:` = session shortcut: its keys are never written to
/// `kglobalshortcutsrc`.
pub const ACTION: &str = "_k_session:claude-cu-abort-escape";
pub const ACTION_FRIENDLY: &str = "Stop Computer Use (Escape)";

pub const KGA_NAME: &str = "org.kde.kglobalaccel";
const KGA_PATH: &str = "/kglobalaccel";
const KGA_IFACE: &str = "org.kde.KGlobalAccel";
const COMPONENT_IFACE: &str = "org.kde.kglobalaccel.Component";
const NO_SUCH_COMPONENT: &str = "org.kde.kglobalaccel.NoSuchComponent";
const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";

/// `Qt::Key_Escape`; a key sequence goes over D-Bus as `(ai)` with four
/// combined key codes, unused ones 0 (kglobalaccel
/// `src/kglobalshortcutinfo_dbus.cpp`).
pub const QT_KEY_ESCAPE: i32 = 0x0100_0000;

/// `KGlobalAccelD::SetShortcutFlag`.
const SET_PRESENT: u32 = 2;
const NO_AUTOLOADING: u32 = 4;

/// `KGlobalAccel::MatchType`, sent as `(i)`: exactly bare Escape.
const MATCH_EQUAL: i32 = 0;
/// `KGlobalAccel::MatchType::Shadows`: every sequence that contains Escape
/// (`Esc, X`, `Ctrl+X, Esc`, `A, Esc, B`). A bare Escape grab would break
/// them: kglobalaccel matches the last key first. (`Shadowed`, 2, can never
/// match a one-key query.)
const MATCH_SHADOWS: i32 = 1;

/// How long `resume` keeps Escape released (and reporting muted) after the
/// caller's injected Escape was sent. Portal key events reach KWin
/// asynchronously after the bridge command returned; re-grabbing earlier
/// would swallow the model's Escape and report it as the user's.
pub const RESUME_SETTLE: Duration = Duration::from_millis(150);

/// Longest a watcher may stay suspended. A real injection is one key press
/// (plus [`RESUME_SETTLE`]); a caller that never sends `resume` would leave
/// Escape released for every profile, so the watcher fails instead (exit 1)
/// and the remaining profiles grab Escape again.
pub const SUSPEND_LIMIT: Duration = Duration::from_secs(5);
/// Watchdog period: suspend limit check and pruning of dead suspenders.
pub const WATCH_TICK: Duration = Duration::from_millis(500);

/// Per D-Bus call (and for connecting to the bus).
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// The listener re-checks its stop flag this often.
const LISTEN_POLL: Duration = Duration::from_millis(200);
/// Owner lookups when kglobalaccel keeps changing owner while arming.
const OWNER_ATTEMPTS: usize = 3;

/// A key sequence as kglobalaccel marshals it: `(ai)`.
pub type KeySeq = (Vec<i32>,);

/// `KGlobalShortcutInfo`: `(ssssssaiai)` = unique name, friendly name,
/// component unique, component friendly, context unique, context friendly,
/// keys, default keys.
pub type ShortcutInfo = (
    String,
    String,
    String,
    String,
    String,
    String,
    Vec<i32>,
    Vec<i32>,
);

/// `Some(since)` while this watcher is suspended.
type SuspendState = Arc<Mutex<Option<Instant>>>;

/// Bare Escape, padded to four keys like `QKeySequence` marshalling.
pub fn escape_seq() -> KeySeq {
    (vec![QT_KEY_ESCAPE, 0, 0, 0],)
}

/// Whether `seq` is exactly bare Escape (trailing zeros allowed).
pub fn is_escape(seq: &KeySeq) -> bool {
    matches!(seq.0.split_first(), Some((&QT_KEY_ESCAPE, rest)) if rest.iter().all(|k| *k == 0))
}

/// Shortcuts of other components among `infos`, as "component: action"
/// (friendly names when set), de-duplicated. Our own component (a leftover
/// from a crashed watcher) is not a conflict.
pub fn foreign_owners(infos: &[ShortcutInfo]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for info in infos {
        if info.2 == COMPONENT {
            continue;
        }
        let component = if info.3.is_empty() { &info.2 } else { &info.3 };
        let action = if info.1.is_empty() { &info.0 } else { &info.1 };
        let label = format!("{component}: {action}");
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// `$XDG_RUNTIME_DIR/claude-cu-esc/kglobalaccel-<bus id>` and its
/// `-suspended` sibling: kglobalaccel is per session bus, so two graphical
/// sessions of one user (sharing the runtime dir) get separate holder sets.
pub fn holder_dirs(runtime_dir: &Path, bus_id: &str) -> (PathBuf, PathBuf) {
    let base = binding_dir(runtime_dir, Mechanism::Kglobalaccel);
    let name = format!("{}-{bus_id}", Mechanism::Kglobalaccel.as_str());
    (
        base.with_file_name(&name),
        base.with_file_name(format!("{name}-suspended")),
    )
}

/// `$XDG_RUNTIME_DIR/claude-cu-esc` must be a real directory owned by us
/// with no group/other access, checked BEFORE anything is created below it
/// (the crate checks only the holder directory itself when given a path).
/// Same rules as the crate's `for_mechanism`: a looser mode on our own
/// directory is tightened to 0700; a symlink or another owner is refused.
pub fn ensure_private_parent(runtime_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let parent = runtime_dir.join(cu_abort_watch::RUNTIME_SUBDIR);
    match std::fs::DirBuilder::new().mode(0o700).create(&parent) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let denied = |why: &str| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{}: {why}", parent.display()),
        )
    };
    let md = std::fs::symlink_metadata(&parent)?;
    if md.file_type().is_symlink() {
        return Err(denied("is a symlink"));
    }
    if !md.is_dir() {
        return Err(denied("is not a directory"));
    }
    // SAFETY: getuid has no memory effects and cannot fail.
    if md.uid() != unsafe { libc::getuid() } {
        return Err(denied("is owned by another user"));
    }
    if md.mode() & 0o077 != 0 {
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        let md = std::fs::symlink_metadata(&parent)?;
        if md.file_type().is_symlink() || md.mode() & 0o077 != 0 {
            return Err(denied("is accessible to other users"));
        }
    }
    Ok(())
}

/// After a failed arm: remove a holder directory nobody uses (no holder
/// file, nothing but `.lock`), under its lock. A directory with any other
/// holder's file is left alone. A process that opened the old `.lock` in the
/// same instant fails its own arm (its holder file has nowhere to go).
pub fn remove_if_unused(set: &SharedBinding) {
    let Ok(lock) = set.lock() else {
        return;
    };
    if lock.others() > 0 || lock.is_registered() {
        return;
    }
    let dir = set.dir();
    let busy = std::fs::read_dir(dir).map_or(true, |entries| {
        entries
            .filter_map(Result::ok)
            .any(|e| e.file_name() != ".lock")
    });
    if busy {
        return;
    }
    let _ = std::fs::remove_file(dir.join(".lock"));
    let _ = std::fs::remove_dir(dir);
    drop(lock);
}

/// A bus id is 32 lowercase hex digits (D-Bus spec, `GetId`).
fn valid_bus_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The kglobalaccel calls the backend makes; a trait so the state machine
/// can be tested without a bus.
pub trait Kglobalaccel {
    /// `globalShortcutsByKey(Escape, matchType)`.
    fn shortcuts_by_key(&self, match_type: i32) -> Result<Vec<ShortcutInfo>, BoxError>;
    /// Action names of [`COMPONENT`]; `None` when the component does not exist.
    fn component_actions(&self) -> Result<Option<Vec<String>>, BoxError>;
    /// `doRegister` of our action id.
    fn do_register(&self) -> Result<(), BoxError>;
    /// `setShortcutKeys(our action, [Escape], flags)`: the keys now active.
    fn set_keys(&self, flags: u32) -> Result<Vec<KeySeq>, BoxError>;
    /// `setInactive` of `action` in [`COMPONENT`]: releases its grab.
    fn set_inactive(&self, action: &str) -> Result<(), BoxError>;
    /// `cleanUp` of [`COMPONENT`]: drops its inactive actions and, when
    /// nothing is left, the component (written out at once).
    fn clean_up(&self) -> Result<bool, BoxError>;
    /// `unregister(COMPONENT, action)`.
    fn unregister(&self, action: &str) -> Result<bool, BoxError>;
    /// Returns once every signal kglobalaccel sent before this call has
    /// been handled by the listener.
    fn barrier(&self) -> Result<(), BoxError>;
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Remove [`COMPONENT`] with every action in it, leaving no entry in
/// kglobalaccel, System Settings or `kglobalshortcutsrc`. Idempotent.
pub fn remove_component<K: Kglobalaccel>(kga: &K) -> Result<(), BoxError> {
    let Some(actions) = kga.component_actions()? else {
        return Ok(());
    };
    for action in &actions {
        kga.set_inactive(action)?;
    }
    kga.clean_up()?;
    // cleanUp only looks at the current context; unregister what is left.
    if let Some(left) = kga.component_actions()? {
        for action in &left {
            kga.unregister(action)?;
        }
    }
    Ok(())
}

/// Grab Escape again (no config write: without NoAutoloading kglobalaccel
/// keeps the action's keys and only marks it present).
fn reactivate<K: Kglobalaccel>(kga: &K) -> Result<(), BoxError> {
    let keys = kga.set_keys(SET_PRESENT)?;
    if keys.is_empty() {
        return Err("the Escape shortcut is gone from kglobalaccel (was it restarted?)".into());
    }
    Ok(())
}

/// Watchdog step for a watcher that is not suspended itself: when the lock
/// pruned a dead suspender and nobody else is suspended, grab Escape again
/// (the dead one released it and will never resume). True when it re-grabbed.
pub fn heal_dead_suspenders<K: Kglobalaccel>(
    kga: &K,
    suspenders: &SharedBinding,
    state: &Mutex<Option<Instant>>,
) -> Result<bool, BoxError> {
    let lock = suspenders.lock()?;
    let suspended = lock_state(state).is_some();
    if lock.pruned() == 0 || lock.others() > 0 || lock.is_registered() || suspended {
        return Ok(false);
    }
    reactivate(kga)?;
    Ok(true)
}

fn lock_state(state: &Mutex<Option<Instant>>) -> std::sync::MutexGuard<'_, Option<Instant>> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

/// The KDE abort watcher's state machine (no bus specifics).
pub struct Core<K: Kglobalaccel> {
    pub kga: K,
    /// Holders of the shared registration.
    pub binding: SharedBinding,
    /// Holders currently suspended (Escape released for an injected key).
    pub suspenders: SharedBinding,
    suspended: SuspendState,
}

impl<K: Kglobalaccel> Core<K> {
    pub fn new(kga: K, binding: SharedBinding, suspenders: SharedBinding) -> Self {
        Self {
            kga,
            binding,
            suspenders,
            suspended: Arc::default(),
        }
    }

    fn suspend_state(&self) -> SuspendState {
        self.suspended.clone()
    }

    #[cfg(test)]
    pub fn is_suspended(&self) -> bool {
        lock_state(&self.suspended).is_some()
    }

    /// Join the holder set; the first holder registers Escape. Refuses when
    /// another component already uses Escape. Leaves nothing behind on `Err`.
    pub fn arm(&mut self) -> Result<(), BoxError> {
        let mut lock = self.binding.lock()?;
        let first = lock.is_first();

        let mut existing = self.kga.component_actions()?;
        if first && existing.is_some() {
            // No live holder: our component is stale (a crashed or SIGKILLed
            // watcher, `lock.pruned() > 0`) and may still grab Escape.
            // Remove it before anything can refuse to arm, so a refusal
            // never leaves it behind.
            remove_component(&self.kga)?;
            existing = None;
        }

        let mut infos = self.kga.shortcuts_by_key(MATCH_EQUAL)?;
        infos.extend(self.kga.shortcuts_by_key(MATCH_SHADOWS)?);
        let foreign = foreign_owners(&infos);
        if !foreign.is_empty() {
            return Err(format!(
                "Escape is already used by a global shortcut ({}); not overriding it. \
                 Free it in System Settings > Keyboard > Shortcuts to stop Computer Use with Escape",
                foreign.join("; ")
            )
            .into());
        }

        let healthy = existing
            .as_ref()
            .is_some_and(|actions| actions.iter().any(|a| a == ACTION));
        if !healthy {
            // First holder, or kglobalaccel lost the registration (restart):
            // register it for every holder.
            if existing.is_some() {
                remove_component(&self.kga)?;
            }
            self.register()?;
        }
        if let Err(e) = lock.register() {
            if first {
                let _ = remove_component(&self.kga);
            }
            return Err(e.into());
        }
        Ok(())
    }

    fn register(&self) -> Result<(), BoxError> {
        self.kga.do_register()?;
        let keys = match self.kga.set_keys(SET_PRESENT | NO_AUTOLOADING) {
            Ok(keys) => keys,
            Err(e) => {
                let _ = remove_component(&self.kga);
                return Err(e);
            }
        };
        if !keys.iter().any(is_escape) {
            let _ = remove_component(&self.kga);
            return Err(format!(
                "kglobalaccel did not give bare Escape to the Computer Use shortcut (active keys: {keys:?}). \
                 Before Plasma 6.7 it drops a key that another shortcut already uses; \
                 check System Settings > Keyboard > Shortcuts for Escape"
            )
            .into());
        }
        Ok(())
    }

    /// Release the Escape grab (for every profile) until the matching
    /// [`Core::resume`].
    pub fn suspend(&mut self) -> Result<(), BoxError> {
        let mut lock = self.suspenders.lock()?;
        lock.register()?;
        lock_state(&self.suspended).get_or_insert_with(Instant::now);
        // Under the lock: the watchdog of another watcher cannot re-grab
        // in between.
        self.kga.set_inactive(ACTION)
    }

    /// Wait [`RESUME_SETTLE`], re-grab Escape unless another profile is
    /// still suspended, then drain the signals already queued.
    pub fn resume(&mut self) -> Result<(), BoxError> {
        if lock_state(&self.suspended).is_none() {
            return Ok(());
        }
        self.kga.sleep(RESUME_SETTLE);
        {
            let mut lock = self.suspenders.lock()?;
            lock.unregister()?;
            *lock_state(&self.suspended) = None;
            if lock.others() == 0 {
                reactivate(&self.kga)?;
            }
        }
        self.kga.barrier()
    }

    /// Leave the holder set; the last holder removes the component.
    /// Best effort: errors go to stderr.
    pub fn disarm(&mut self) {
        let was_suspended = lock_state(&self.suspended).take().is_some();
        let kga = &self.kga;
        let last = match self.binding.release(|| {
            if let Err(e) = remove_component(kga) {
                log(&format!("removing the kglobalaccel component failed: {e}"));
            }
        }) {
            Ok(last) => last,
            Err(e) => {
                // The crate already ran the unbind unless it saw another
                // live holder: never unbind or re-grab after this.
                log(&format!("holder set: {e}"));
                true
            }
        };
        if was_suspended {
            match self.suspenders.lock() {
                Ok(mut lock) => {
                    let _ = lock.unregister();
                    if !last
                        && lock.others() == 0
                        && let Err(e) = reactivate(&self.kga)
                    {
                        log(&format!(
                            "re-grabbing Escape for the other profiles failed: {e}"
                        ));
                    }
                }
                Err(e) => log(&format!("suspend set: {e}")),
            }
        }
    }
}

fn log(message: &str) {
    eprintln!("[kwin-portal-bridge] abort-watch: {message}");
}

fn dbus_err(context: &str, e: dbus::Error) -> BoxError {
    format!(
        "{context}: {}",
        e.message().or(e.name()).unwrap_or("D-Bus error")
    )
    .into()
}

/// A private session-bus connection within [`CALL_TIMEOUT`] (libdbus has no
/// connect timeout; a hung bus must not hang arming).
fn connect_bounded() -> Result<Connection, BoxError> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("session-bus-connect".to_owned())
        .spawn(move || {
            let _ = tx.send(Connection::new_session());
        })
        .map_err(|e| format!("cannot start the bus connection thread: {e}"))?;
    match rx.recv_timeout(CALL_TIMEOUT) {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(dbus_err("cannot connect to the session bus", e)),
        Err(_) => Err(format!(
            "the session bus did not answer within {} ms",
            CALL_TIMEOUT.as_millis()
        )
        .into()),
    }
}

/// One method call without auto-start, bounded by [`CALL_TIMEOUT`].
fn call(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    member: &str,
    args: impl FnOnce(Message) -> Message,
) -> Result<Message, dbus::Error> {
    let mut msg = Message::new_method_call(dest, path, iface, member)
        .map_err(|e| dbus::Error::new_failed(&e))?;
    msg.set_auto_start(false);
    conn.send_with_reply_and_block(args(msg), CALL_TIMEOUT)
}

fn bus_call(
    conn: &Connection,
    member: &str,
    args: impl FnOnce(Message) -> Message,
) -> Result<Message, dbus::Error> {
    call(conn, BUS_NAME, BUS_PATH, BUS_NAME, member, args)
}

/// The bus id (`org.freedesktop.DBus.GetId`), checked to be 32 hex digits
/// since it becomes part of a path.
fn bus_id(conn: &Connection) -> Result<String, BoxError> {
    let id: String = bus_call(conn, "GetId", |m| m)
        .map_err(|e| dbus_err("session bus GetId", e))?
        .read1()
        .map_err(|e| format!("session bus GetId: {e}"))?;
    if !valid_bus_id(&id) {
        return Err(format!("session bus GetId returned an unexpected id {id:?}").into());
    }
    Ok(id)
}

/// The unique name owning `org.kde.kglobalaccel`, or why there is none.
fn kglobalaccel_owner(conn: &Connection) -> Result<String, BoxError> {
    match bus_call(conn, "GetNameOwner", |m| m.append1(KGA_NAME)) {
        Ok(reply) => Ok(reply.read1::<String>().map_err(|e| e.to_string())?),
        Err(e) if e.name() == Some("org.freedesktop.DBus.Error.NameHasNoOwner") => Err(format!(
            "{KGA_NAME} is not on the session bus (no KDE global shortcut daemon): no global Escape"
        )
        .into()),
        Err(e) => Err(dbus_err("session bus GetNameOwner", e)),
    }
}

/// kglobalaccel over a blocking session-bus connection. Every call is sent
/// without auto-start, so a stopped kglobalaccel is never started by us.
pub struct DbusKga {
    conn: Connection,
    /// Unique name of the listener connection (for [`Kglobalaccel::barrier`]).
    listener: Option<String>,
}

impl DbusKga {
    fn kga(
        &self,
        member: &str,
        args: impl FnOnce(Message) -> Message,
    ) -> Result<Message, BoxError> {
        call(&self.conn, KGA_NAME, KGA_PATH, KGA_IFACE, member, args)
            .map_err(|e| dbus_err(&format!("kglobalaccel {member}"), e))
    }

    fn action_id(action: &str) -> Vec<String> {
        vec![
            COMPONENT.to_owned(),
            action.to_owned(),
            COMPONENT_FRIENDLY.to_owned(),
            ACTION_FRIENDLY.to_owned(),
        ]
    }

    fn component_path(&self) -> Result<Option<dbus::Path<'static>>, BoxError> {
        match call(
            &self.conn,
            KGA_NAME,
            KGA_PATH,
            KGA_IFACE,
            "getComponent",
            |m| m.append1(COMPONENT),
        ) {
            Ok(reply) => Ok(Some(
                reply
                    .read1::<dbus::Path>()
                    .map_err(|e| format!("kglobalaccel getComponent: {e}"))?
                    .into_static(),
            )),
            Err(e) if e.name() == Some(NO_SUCH_COMPONENT) => Ok(None),
            Err(e) => Err(dbus_err("kglobalaccel getComponent", e)),
        }
    }
}

impl Kglobalaccel for DbusKga {
    fn shortcuts_by_key(&self, match_type: i32) -> Result<Vec<ShortcutInfo>, BoxError> {
        let reply = self.kga("globalShortcutsByKey", |m| {
            m.append2(escape_seq(), (match_type,))
        })?;
        Ok(reply
            .read1::<Vec<ShortcutInfo>>()
            .map_err(|e| format!("kglobalaccel globalShortcutsByKey: {e}"))?)
    }

    fn component_actions(&self) -> Result<Option<Vec<String>>, BoxError> {
        let Some(path) = self.component_path()? else {
            return Ok(None);
        };
        let reply = match call(
            &self.conn,
            KGA_NAME,
            &path,
            COMPONENT_IFACE,
            "shortcutNames",
            |m| m,
        ) {
            Ok(reply) => reply,
            // Removed between the two calls.
            Err(e) if e.name() == Some("org.freedesktop.DBus.Error.UnknownObject") => {
                return Ok(None);
            }
            Err(e) => return Err(dbus_err("kglobalaccel shortcutNames", e)),
        };
        Ok(Some(
            reply
                .read1::<Vec<String>>()
                .map_err(|e| format!("kglobalaccel shortcutNames: {e}"))?,
        ))
    }

    fn do_register(&self) -> Result<(), BoxError> {
        self.kga("doRegister", |m| m.append1(Self::action_id(ACTION)))?;
        Ok(())
    }

    fn set_keys(&self, flags: u32) -> Result<Vec<KeySeq>, BoxError> {
        let reply = self.kga("setShortcutKeys", |m| {
            m.append3(Self::action_id(ACTION), vec![escape_seq()], flags)
        })?;
        Ok(reply
            .read1::<Vec<KeySeq>>()
            .map_err(|e| format!("kglobalaccel setShortcutKeys: {e}"))?)
    }

    fn set_inactive(&self, action: &str) -> Result<(), BoxError> {
        self.kga("setInactive", |m| m.append1(Self::action_id(action)))?;
        Ok(())
    }

    fn clean_up(&self) -> Result<bool, BoxError> {
        let Some(path) = self.component_path()? else {
            return Ok(false);
        };
        let reply = call(
            &self.conn,
            KGA_NAME,
            &path,
            COMPONENT_IFACE,
            "cleanUp",
            |m| m,
        )
        .map_err(|e| dbus_err("kglobalaccel cleanUp", e))?;
        Ok(reply.read1::<bool>().unwrap_or(false))
    }

    fn unregister(&self, action: &str) -> Result<bool, BoxError> {
        let reply = self.kga("unregister", |m| m.append2(COMPONENT, action))?;
        Ok(reply.read1::<bool>().unwrap_or(false))
    }

    fn barrier(&self) -> Result<(), BoxError> {
        // kglobalaccel sent its press signals before the setShortcutKeys
        // reply we already have; the bus queues them to the listener before
        // this ping, which the listener answers from the same queue (dbus-rs
        // default reply). This only drains presses that happened before the
        // re-grab; a late injected Escape is kept out by RESUME_SETTLE.
        let Some(listener) = &self.listener else {
            return Ok(());
        };
        call(
            &self.conn,
            listener,
            "/",
            "org.freedesktop.DBus.Peer",
            "Ping",
            |m| m,
        )
        .map(|_| ())
        .map_err(|e| dbus_err("listener barrier", e))
    }
}

/// `AddMatch` with our own timeout (dbus-rs uses 5 s).
fn add_match(conn: &Connection, rule: &str) -> Result<(), BoxError> {
    bus_call(conn, "AddMatch", |m| m.append1(rule))
        .map(|_| ())
        .map_err(|e| dbus_err("session bus AddMatch", e))
}

/// The listening connection: `globalShortcutPressed` of our action ->
/// `sink.abort()`; kglobalaccel leaving or the bus closing -> `sink.fail()`.
struct Listener {
    stop: Arc<AtomicBool>,
    name: String,
}

impl Listener {
    fn start(owner: &str, sink: AbortSink) -> Result<Self, BoxError> {
        let conn = connect_bounded()?;
        let name = conn.unique_name().to_string();

        let pressed = MatchRule::new_signal(COMPONENT_IFACE, "globalShortcutPressed")
            .with_sender(owner.to_owned());
        add_match(
            &conn,
            &format!("{},arg0='{COMPONENT}'", pressed.match_str()),
        )?;
        let on_press = sink.clone();
        conn.start_receive(
            pressed,
            Box::new(move |msg, _| {
                if let Ok((component, action)) = msg.read2::<String, String>()
                    && component == COMPONENT
                    && action == ACTION
                {
                    on_press.abort();
                }
                true
            }),
        );

        let owner_changed =
            MatchRule::new_signal(BUS_NAME, "NameOwnerChanged").with_sender(BUS_NAME);
        add_match(
            &conn,
            &format!("{},arg0='{KGA_NAME}'", owner_changed.match_str()),
        )?;
        let on_owner = sink.clone();
        conn.start_receive(
            owner_changed,
            Box::new(move |msg, _| {
                if let Ok((name, _old, new)) = msg.read3::<String, String, String>()
                    && name == KGA_NAME
                {
                    on_owner.fail(if new.is_empty() {
                        "kglobalaccel left the session bus"
                    } else {
                        "kglobalaccel was restarted: the Escape shortcut is gone"
                    });
                }
                true
            }),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        std::thread::Builder::new()
            .name("kglobalaccel-listener".to_owned())
            .spawn(move || {
                while !stop_flag.load(Ordering::Acquire) {
                    if let Err(e) = conn.process(LISTEN_POLL) {
                        if !stop_flag.load(Ordering::Acquire) {
                            sink.fail(format!(
                                "session bus connection lost: {}",
                                e.message().unwrap_or("disconnected")
                            ));
                        }
                        return;
                    }
                }
            })
            .map_err(|e| format!("cannot start the kglobalaccel listener: {e}"))?;
        Ok(Self { stop, name })
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Bounds suspensions: fails this watcher after [`SUSPEND_LIMIT`] without
/// `resume`, and grabs Escape again when a suspended watcher of another
/// profile died (its own connection, so the control thread is never needed).
struct Watchdog {
    stop: Arc<AtomicBool>,
}

impl Watchdog {
    fn start(
        kga: DbusKga,
        suspenders: SharedBinding,
        state: SuspendState,
        sink: AbortSink,
    ) -> Result<Self, BoxError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        std::thread::Builder::new()
            .name("abort-watch-watchdog".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(WATCH_TICK);
                    if stop_flag.load(Ordering::Acquire) {
                        return;
                    }
                    let since = *lock_state(&state);
                    if let Some(since) = since {
                        if since.elapsed() > SUSPEND_LIMIT {
                            sink.fail(format!(
                                "suspended for more than {} s without resume",
                                SUSPEND_LIMIT.as_secs()
                            ));
                            return;
                        }
                        continue;
                    }
                    match heal_dead_suspenders(&kga, &suspenders, &state) {
                        Ok(true) => log("a suspended watcher died: Escape grabbed again"),
                        Ok(false) => {}
                        Err(e) => log(&format!("watchdog: {e}")),
                    }
                }
            })
            .map_err(|e| format!("cannot start the abort-watch watchdog: {e}"))?;
        Ok(Self { stop })
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// The `abort-watch` backend for `cu_abort_watch::run`.
#[derive(Default)]
pub struct KdeBackend {
    core: Option<Core<DbusKga>>,
    listener: Option<Listener>,
    watchdog: Option<Watchdog>,
}

impl KdeBackend {
    /// Subscribe to the current kglobalaccel owner and confirm it did not
    /// change meanwhile (a restart between `GetNameOwner` and `AddMatch`
    /// would leave a stale sender filter and a missed `NameOwnerChanged`).
    fn listen(conn: &Connection, sink: &AbortSink) -> Result<Listener, BoxError> {
        for _ in 0..OWNER_ATTEMPTS {
            let owner = kglobalaccel_owner(conn)?;
            let listener = Listener::start(&owner, sink.clone())?;
            if kglobalaccel_owner(conn)? == owner {
                return Ok(listener);
            }
            listener.stop();
        }
        Err("kglobalaccel keeps changing its bus owner; not arming".into())
    }
}

impl Backend for KdeBackend {
    fn arm(&mut self, sink: AbortSink) -> Result<Ready, BoxError> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or("XDG_RUNTIME_DIR is not set")?;
        let conn = connect_bounded()?;
        let id = bus_id(&conn)?;
        // Cheap refusal first: no holder directory for a desktop without
        // kglobalaccel.
        kglobalaccel_owner(&conn)?;
        let (binding_path, suspend_path) = holder_dirs(&runtime_dir, &id);
        ensure_private_parent(&runtime_dir)?;
        let binding = SharedBinding::in_dir(binding_path)?;
        let suspenders = match SharedBinding::in_dir(suspend_path) {
            Ok(set) => set,
            Err(e) => {
                remove_if_unused(&binding);
                return Err(e.into());
            }
        };
        let fail = |e: BoxError, binding: &SharedBinding, suspenders: &SharedBinding| {
            remove_if_unused(binding);
            remove_if_unused(suspenders);
            e
        };
        // Subscribe before registering: no press is lost.
        let listener = match Self::listen(&conn, &sink) {
            Ok(l) => l,
            Err(e) => return Err(fail(e, &binding, &suspenders)),
        };
        let kga = DbusKga {
            conn,
            listener: Some(listener.name.clone()),
        };
        let mut core = Core::new(kga, binding, suspenders);
        if let Err(e) = core.arm() {
            listener.stop();
            return Err(fail(e, &core.binding, &core.suspenders));
        }
        let watchdog = connect_bounded().and_then(|conn| {
            Watchdog::start(
                DbusKga {
                    conn,
                    listener: None,
                },
                core.suspenders.clone(),
                core.suspend_state(),
                sink,
            )
        });
        let watchdog = match watchdog {
            Ok(w) => w,
            Err(e) => {
                listener.stop();
                core.disarm();
                return Err(fail(e, &core.binding, &core.suspenders));
            }
        };
        self.core = Some(core);
        self.listener = Some(listener);
        self.watchdog = Some(watchdog);
        Ok(Ready {
            mechanism: Mechanism::Kglobalaccel,
            synthetic_safe: false,
        })
    }

    fn suspend(&mut self) -> Result<(), BoxError> {
        match &mut self.core {
            Some(core) => core.suspend(),
            None => Ok(()),
        }
    }

    fn resume(&mut self) -> Result<(), BoxError> {
        match &mut self.core {
            Some(core) => core.resume(),
            None => Ok(()),
        }
    }

    fn disarm(&mut self) {
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.stop();
        }
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
        if let Some(mut core) = self.core.take() {
            core.disarm();
        }
    }
}

/// `kwin-portal-bridge abort-watch`: runs the shared watcher off the async
/// runtime (it blocks and owns stdin, stdout and SIGTERM/SIGINT/SIGHUP).
pub async fn run() -> std::process::ExitCode {
    tokio::task::spawn_blocking(|| cu_abort_watch::run(KdeBackend::default()))
        .await
        .unwrap_or(std::process::ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[test]
    fn escape_is_qt_key_escape_padded_to_four_keys() {
        assert_eq!(QT_KEY_ESCAPE, 0x0100_0000);
        assert_eq!(escape_seq(), (vec![0x0100_0000, 0, 0, 0],));
        assert!(is_escape(&escape_seq()));
        assert!(is_escape(&(vec![QT_KEY_ESCAPE],)));
        // Shift+Escape, Escape followed by another key, nothing.
        assert!(!is_escape(&(vec![QT_KEY_ESCAPE | 0x0200_0000, 0, 0, 0],)));
        assert!(!is_escape(&(vec![QT_KEY_ESCAPE, 0x41, 0, 0],)));
        assert!(!is_escape(&(vec![],)));
    }

    const BUS_ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn holder_dirs_are_keyed_by_the_session_bus() {
        let (binding, suspended) = holder_dirs(Path::new("/run/user/1"), BUS_ID);
        assert_eq!(
            binding,
            PathBuf::from(format!("/run/user/1/claude-cu-esc/kglobalaccel-{BUS_ID}"))
        );
        assert_eq!(
            suspended,
            PathBuf::from(format!(
                "/run/user/1/claude-cu-esc/kglobalaccel-{BUS_ID}-suspended"
            ))
        );
        assert!(valid_bus_id(BUS_ID));
        for bad in ["", "../x", "0123456789abcdef0123456789abcdeg", "0123"] {
            assert!(!valid_bus_id(bad), "{bad}");
        }
    }

    /// `QKeySequence::matches`: 0 no match, 1 partial (prefix), 2 exact.
    fn qmatches(a: &[i32], b: &[i32]) -> u8 {
        if a.len() > b.len() || a != &b[..a.len()] {
            0
        } else if a.len() == b.len() {
            2
        } else {
            1
        }
    }

    /// kglobalacceld `Utils::contains` (sequencehelpers_p.cpp).
    fn seq_contains(key: &[i32], other: &[i32]) -> bool {
        let min = key.len().min(other.len());
        if min == 0 {
            return false;
        }
        let rev = |s: &[i32]| s.iter().rev().copied().collect::<Vec<_>>();
        (0..=other.len() - min).any(|i| {
            let cropped = &other[i..];
            qmatches(key, cropped) == 1 || qmatches(&rev(key), &rev(cropped)) == 1
        })
    }

    /// `GlobalShortcutContext::getShortcutsByKey` for a query `key`.
    fn by_key(match_type: i32, key: &[i32], other: &[i32]) -> bool {
        match match_type {
            MATCH_EQUAL => key == other,
            MATCH_SHADOWS => !other.is_empty() && seq_contains(key, other),
            2 => !other.is_empty() && seq_contains(other, key),
            _ => false,
        }
    }

    const CTRL: i32 = 0x0400_0000;
    const SHIFT: i32 = 0x0200_0000;
    const X: i32 = 0x58;
    const A: i32 = 0x41;
    const B: i32 = 0x42;

    #[test]
    fn match_types_find_every_sequence_containing_escape() {
        let esc = [QT_KEY_ESCAPE];
        assert!(by_key(MATCH_EQUAL, &esc, &[QT_KEY_ESCAPE]));
        assert!(!by_key(MATCH_SHADOWS, &esc, &[QT_KEY_ESCAPE]));
        for seq in [
            vec![QT_KEY_ESCAPE, X],
            vec![CTRL | X, QT_KEY_ESCAPE],
            vec![A, QT_KEY_ESCAPE, B],
        ] {
            assert!(by_key(MATCH_SHADOWS, &esc, &seq), "{seq:x?}");
            assert!(!by_key(MATCH_EQUAL, &esc, &seq), "{seq:x?}");
            // Shadowed (2) never matches a one-key query.
            assert!(!by_key(2, &esc, &seq), "{seq:x?}");
        }
        for seq in [vec![SHIFT | QT_KEY_ESCAPE], vec![A, B], vec![CTRL | X, X]] {
            assert!(!by_key(MATCH_EQUAL, &esc, &seq) && !by_key(MATCH_SHADOWS, &esc, &seq));
        }
    }

    fn info(component: &str, component_friendly: &str, action_friendly: &str) -> ShortcutInfo {
        (
            "act".into(),
            action_friendly.into(),
            component.into(),
            component_friendly.into(),
            "default".into(),
            "Default Context".into(),
            vec![QT_KEY_ESCAPE],
            vec![],
        )
    }

    #[test]
    fn conflicts_ignore_our_own_component_and_name_the_others() {
        assert!(foreign_owners(&[]).is_empty());
        assert!(foreign_owners(&[info(COMPONENT, COMPONENT_FRIENDLY, ACTION_FRIENDLY)]).is_empty());
        let infos = [
            info(COMPONENT, "", ""),
            info("org.kde.spectacle.desktop", "Spectacle", "Cancel"),
            info("org.kde.spectacle.desktop", "Spectacle", "Cancel"),
            info("myscript", "", ""),
        ];
        assert_eq!(
            foreign_owners(&infos),
            vec!["Spectacle: Cancel".to_owned(), "myscript: act".to_owned()]
        );
    }

    /// action -> (keys, present)
    type Actions = BTreeMap<String, (Vec<KeySeq>, bool)>;

    /// An in-memory kglobalaccel with the daemon's semantics for the calls
    /// we make (kglobalacceld `kglobalacceld.cpp`, `globalshortcut.cpp`,
    /// `component.cpp`), recording every call.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        /// action -> (keys, present)
        actions: RefCell<Option<Actions>>,
        /// (key sequence, info) of other components' shortcuts.
        foreign: RefCell<Vec<(Vec<i32>, ShortcutInfo)>>,
        refuse_escape: bool,
    }

    impl Fake {
        fn call(&self, c: impl Into<String>) {
            self.calls.borrow_mut().push(c.into());
        }
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.borrow_mut())
        }
        /// Is Escape grabbed by us right now?
        fn grabbed(&self) -> bool {
            self.actions.borrow().as_ref().is_some_and(|a| {
                a.values()
                    .any(|(keys, present)| *present && keys.iter().any(is_escape))
            })
        }
    }

    impl Kglobalaccel for Fake {
        fn shortcuts_by_key(&self, m: i32) -> Result<Vec<ShortcutInfo>, BoxError> {
            self.call(format!("byKey({m})"));
            let esc = [QT_KEY_ESCAPE];
            let mut out: Vec<ShortcutInfo> = self
                .foreign
                .borrow()
                .iter()
                .filter(|(seq, _)| by_key(m, &esc, seq))
                .map(|(_, info)| info.clone())
                .collect();
            let ours = self
                .actions
                .borrow()
                .as_ref()
                .is_some_and(|a| a.values().any(|(keys, _)| keys.iter().any(is_escape)));
            if m == MATCH_EQUAL && ours {
                out.push(info(COMPONENT, COMPONENT_FRIENDLY, ACTION_FRIENDLY));
            }
            Ok(out)
        }
        fn component_actions(&self) -> Result<Option<Vec<String>>, BoxError> {
            Ok(self
                .actions
                .borrow()
                .as_ref()
                .map(|a| a.keys().cloned().collect()))
        }
        fn do_register(&self) -> Result<(), BoxError> {
            self.call("doRegister");
            self.actions
                .borrow_mut()
                .get_or_insert_default()
                .entry(ACTION.to_owned())
                .or_insert((vec![], false));
            Ok(())
        }
        fn set_keys(&self, flags: u32) -> Result<Vec<KeySeq>, BoxError> {
            self.call(format!("setShortcutKeys({flags})"));
            let mut actions = self.actions.borrow_mut();
            let Some(entry) = actions.as_mut().and_then(|a| a.get_mut(ACTION)) else {
                return Ok(vec![]);
            };
            if flags & NO_AUTOLOADING != 0 {
                entry.0 = if self.refuse_escape {
                    vec![]
                } else {
                    vec![escape_seq()]
                };
            }
            if flags & SET_PRESENT != 0 {
                entry.1 = true;
            }
            Ok(entry.0.clone())
        }
        fn set_inactive(&self, action: &str) -> Result<(), BoxError> {
            self.call(format!("setInactive({action})"));
            if let Some(entry) = self
                .actions
                .borrow_mut()
                .as_mut()
                .and_then(|a| a.get_mut(action))
            {
                entry.1 = false;
            }
            Ok(())
        }
        fn clean_up(&self) -> Result<bool, BoxError> {
            self.call("cleanUp");
            let mut actions = self.actions.borrow_mut();
            let Some(a) = actions.as_mut() else {
                return Ok(false);
            };
            let before = a.len();
            a.retain(|_, (_, present)| *present);
            let changed = a.len() != before;
            if a.is_empty() {
                *actions = None;
            }
            Ok(changed)
        }
        fn unregister(&self, action: &str) -> Result<bool, BoxError> {
            self.call(format!("unregister({action})"));
            Ok(self
                .actions
                .borrow_mut()
                .as_mut()
                .is_some_and(|a| a.remove(action).is_some()))
        }
        fn barrier(&self) -> Result<(), BoxError> {
            self.call("barrier");
            Ok(())
        }
        fn sleep(&self, d: Duration) {
            // The grab state during the settle window is what matters.
            self.call(format!(
                "sleep({}ms, grabbed={})",
                d.as_millis(),
                self.grabbed()
            ));
        }
    }

    struct Env {
        dir: tempfile::TempDir,
    }

    impl Env {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn core(&self, kga: Fake) -> Core<Fake> {
            self.core_for(kga, std::process::id())
        }
        fn core_for(&self, kga: Fake, pid: u32) -> Core<Fake> {
            let (binding, suspended) = holder_dirs(self.dir.path(), BUS_ID);
            Core::new(
                kga,
                SharedBinding::with_holder(binding, pid).unwrap(),
                SharedBinding::with_holder(suspended, pid).unwrap(),
            )
        }
        fn suspenders_of(&self, pid: u32) -> SharedBinding {
            SharedBinding::with_holder(holder_dirs(self.dir.path(), BUS_ID).1, pid).unwrap()
        }
    }

    #[test]
    fn arm_registers_bare_escape_once_and_disarm_removes_the_component() {
        let env = Env::new();
        let mut core = env.core(Fake::default());
        core.arm().unwrap();
        assert_eq!(
            core.kga.calls(),
            ["byKey(0)", "byKey(1)", "doRegister", "setShortcutKeys(6)"]
        );
        assert!(core.kga.grabbed());
        core.disarm();
        assert_eq!(
            core.kga.calls(),
            [format!("setInactive({ACTION})"), "cleanUp".into()]
        );
        assert!(core.kga.actions.borrow().is_none());
    }

    #[test]
    fn a_foreign_escape_shortcut_refuses_to_arm_and_registers_nothing() {
        let env = Env::new();
        let fake = Fake::default();
        fake.foreign
            .borrow_mut()
            .push((vec![QT_KEY_ESCAPE], info("kwin", "KWin", "Close Window")));
        let mut core = env.core(fake);
        let err = core.arm().unwrap_err().to_string();
        assert!(err.contains("KWin: Close Window"), "{err}");
        assert_eq!(core.kga.calls(), ["byKey(0)", "byKey(1)"]);
        assert!(core.kga.actions.borrow().is_none());
        // Not a holder: a later watcher is first again.
        assert!(core.binding.lock().unwrap().is_first());
    }

    #[test]
    fn rejected_escape_is_an_arm_error_with_nothing_left() {
        let env = Env::new();
        let mut core = env.core(Fake {
            refuse_escape: true,
            ..Fake::default()
        });
        let err = core.arm().unwrap_err().to_string();
        assert!(err.contains("did not give bare Escape"), "{err}");
        assert!(err.contains("Plasma 6.7"), "{err}");
        assert!(core.kga.actions.borrow().is_none());
        assert!(core.binding.lock().unwrap().is_first());
    }

    #[test]
    fn a_leftover_component_is_removed_before_the_first_holder_registers() {
        let env = Env::new();
        let fake = Fake::default();
        // A SIGKILLed watcher's action, still present (grabbing), plus an
        // older action name.
        *fake.actions.borrow_mut() = Some(BTreeMap::from([
            (ACTION.to_owned(), (vec![escape_seq()], true)),
            ("old".to_owned(), (vec![], false)),
        ]));
        let mut core = env.core(fake);
        core.arm().unwrap();
        assert_eq!(
            core.kga.calls(),
            [
                format!("setInactive({ACTION})"),
                "setInactive(old)".into(),
                "cleanUp".into(),
                "byKey(0)".into(),
                "byKey(1)".into(),
                "doRegister".into(),
                "setShortcutKeys(6)".into(),
            ]
        );
        assert!(core.kga.grabbed());
    }

    #[test]
    fn suspend_releases_and_resume_regrabs_after_the_settle_window() {
        let env = Env::new();
        let mut core = env.core(Fake::default());
        core.arm().unwrap();
        core.kga.calls();

        core.suspend().unwrap();
        assert!(core.is_suspended());
        assert!(!core.kga.grabbed());
        assert_eq!(core.kga.calls(), [format!("setInactive({ACTION})")]);

        core.resume().unwrap();
        assert!(!core.is_suspended());
        assert!(core.kga.grabbed());
        // Settle with Escape still released, then re-grab without
        // NoAutoloading (no config write), then drain queued signals.
        assert_eq!(
            core.kga.calls(),
            [
                format!("sleep({}ms, grabbed=false)", RESUME_SETTLE.as_millis()),
                "setShortcutKeys(2)".into(),
                "barrier".into(),
            ]
        );
        assert_eq!(RESUME_SETTLE, Duration::from_millis(150));

        // resume while not suspended: nothing.
        core.resume().unwrap();
        assert!(core.kga.calls().is_empty());
    }

    #[test]
    fn resume_fails_when_kglobalaccel_lost_the_action() {
        let env = Env::new();
        let mut core = env.core(Fake::default());
        core.arm().unwrap();
        core.suspend().unwrap();
        *core.kga.actions.borrow_mut() = None; // kglobalaccel restarted
        let err = core.resume().unwrap_err().to_string();
        assert!(err.contains("gone"), "{err}");
    }

    #[test]
    fn another_suspended_profile_keeps_escape_released() {
        let env = Env::new();
        // A live helper process stands in for the other profile's watcher.
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut core = env.core(Fake::default());
        core.arm().unwrap();
        let theirs = env.suspenders_of(other.id());
        core.suspend().unwrap();
        theirs.lock().unwrap().register().unwrap();
        core.kga.calls();

        core.resume().unwrap();
        assert!(!core.kga.grabbed(), "the other profile is still suspended");
        assert!(
            !core
                .kga
                .calls()
                .iter()
                .any(|c| c.starts_with("setShortcutKeys"))
        );

        // Their watcher died while suspended: pruned, the next resume grabs.
        other.kill().unwrap();
        other.wait().unwrap();
        core.suspend().unwrap();
        core.resume().unwrap();
        assert!(core.kga.grabbed());
    }

    #[test]
    fn second_holder_reuses_the_registration_and_the_last_one_removes_it() {
        let env = Env::new();
        let mut helper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        // First holder = the helper (pid), already registered.
        let mut first = env.core_for(Fake::default(), helper.id());
        first.arm().unwrap();
        let state = first.kga.actions.borrow().clone();

        let fake = Fake::default();
        *fake.actions.borrow_mut() = state;
        let mut second = env.core(fake);
        second.arm().unwrap();
        assert_eq!(
            second.kga.calls(),
            ["byKey(0)", "byKey(1)"],
            "no re-registration"
        );

        // Second leaves while the first still holds: component stays.
        second.disarm();
        assert!(second.kga.calls().is_empty());
        assert!(second.kga.grabbed());

        // The first holder dies (SIGKILL): the next arm finds it pruned and
        // replaces its stale registration.
        helper.kill().unwrap();
        helper.wait().unwrap();
        second.arm().unwrap();
        let calls = second.kga.calls();
        assert!(calls.contains(&"cleanUp".to_owned()), "{calls:?}");
        assert!(calls.ends_with(&["doRegister".into(), "setShortcutKeys(6)".into()]));
        second.disarm();
        assert!(second.kga.actions.borrow().is_none());
    }

    #[test]
    fn a_missing_registration_is_healed_by_a_joining_holder() {
        let env = Env::new();
        let mut helper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut first = env.core_for(Fake::default(), helper.id());
        first.arm().unwrap();
        // kglobalaccel restarted: our component is gone.
        let mut second = env.core(Fake::default());
        second.arm().unwrap();
        assert_eq!(
            second.kga.calls(),
            ["byKey(0)", "byKey(1)", "doRegister", "setShortcutKeys(6)"]
        );
        helper.kill().unwrap();
        helper.wait().unwrap();
    }

    #[test]
    fn disarm_while_suspended_regrabs_for_the_remaining_holders() {
        let env = Env::new();
        let mut helper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut first = env.core_for(Fake::default(), helper.id());
        first.arm().unwrap();
        let fake = Fake::default();
        *fake.actions.borrow_mut() = first.kga.actions.borrow().clone();
        let mut second = env.core(fake);
        second.arm().unwrap();
        second.suspend().unwrap();
        assert!(!second.kga.grabbed());
        second.kga.calls();
        second.disarm();
        assert_eq!(second.kga.calls(), ["setShortcutKeys(2)"]);
        assert!(second.kga.grabbed());
        helper.kill().unwrap();
        helper.wait().unwrap();
    }

    #[test]
    fn a_multi_key_sequence_containing_escape_refuses_to_arm() {
        for seq in [
            vec![QT_KEY_ESCAPE, X],
            vec![CTRL | X, QT_KEY_ESCAPE],
            vec![A, QT_KEY_ESCAPE, B],
        ] {
            let env = Env::new();
            let fake = Fake::default();
            fake.foreign.borrow_mut().push((
                seq.clone(),
                info("org.example.editor", "Editor", "Leave mode"),
            ));
            let mut core = env.core(fake);
            let err = core.arm().unwrap_err().to_string();
            assert!(err.contains("Editor: Leave mode"), "{seq:x?}: {err}");
            assert!(core.kga.actions.borrow().is_none());
        }
    }

    #[test]
    fn a_stale_grab_is_removed_even_when_arming_is_then_refused() {
        // A SIGKILLed watcher left its grab; meanwhile an app took Escape.
        let env = Env::new();
        let fake = Fake::default();
        *fake.actions.borrow_mut() = Some(BTreeMap::from([(
            ACTION.to_owned(),
            (vec![escape_seq()], true),
        )]));
        fake.foreign.borrow_mut().push((
            vec![QT_KEY_ESCAPE],
            info("org.example.app", "Example", "Cancel"),
        ));
        let mut core = env.core(fake);
        let err = core.arm().unwrap_err().to_string();
        assert!(err.contains("Example: Cancel"), "{err}");
        assert!(core.kga.actions.borrow().is_none(), "stale grab removed");
        assert!(!core.kga.grabbed());
        assert!(core.binding.lock().unwrap().is_first());
    }

    #[test]
    fn a_dead_suspender_is_pruned_and_escape_grabbed_again() {
        let env = Env::new();
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut core = env.core(Fake::default());
        core.arm().unwrap();
        // The other profile suspends (Escape released) ...
        env.suspenders_of(other.id())
            .lock()
            .unwrap()
            .register()
            .unwrap();
        core.kga.set_inactive(ACTION).unwrap();
        let state = core.suspend_state();
        assert!(!heal_dead_suspenders(&core.kga, &core.suspenders, &state).unwrap());
        assert!(!core.kga.grabbed(), "still suspended by a live watcher");
        // ... and dies without resuming.
        other.kill().unwrap();
        other.wait().unwrap();
        assert!(heal_dead_suspenders(&core.kga, &core.suspenders, &state).unwrap());
        assert!(core.kga.grabbed());
        // Nothing more to do on the next tick.
        assert!(!heal_dead_suspenders(&core.kga, &core.suspenders, &state).unwrap());
    }

    #[test]
    fn the_watchdog_never_regrabs_while_this_watcher_is_suspended() {
        let env = Env::new();
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut core = env.core(Fake::default());
        core.arm().unwrap();
        env.suspenders_of(other.id())
            .lock()
            .unwrap()
            .register()
            .unwrap();
        core.suspend().unwrap();
        other.kill().unwrap();
        other.wait().unwrap();
        let state = core.suspend_state();
        assert!(!heal_dead_suspenders(&core.kga, &core.suspenders, &state).unwrap());
        assert!(!core.kga.grabbed());
        assert_eq!(SUSPEND_LIMIT, Duration::from_secs(5));
    }

    #[test]
    fn unused_holder_dirs_are_removed_but_never_a_live_holders() {
        let env = Env::new();
        let (dir, _) = holder_dirs(env.dir.path(), BUS_ID);
        let mine = SharedBinding::with_holder(&dir, std::process::id()).unwrap();
        drop(mine.lock().unwrap());
        remove_if_unused(&mine);
        assert!(!dir.exists(), "unused: removed");

        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let theirs = SharedBinding::with_holder(&dir, other.id()).unwrap();
        theirs.lock().unwrap().register().unwrap();
        let mine = SharedBinding::with_holder(&dir, std::process::id()).unwrap();
        remove_if_unused(&mine);
        assert!(
            dir.join(other.id().to_string()).exists(),
            "live holder kept"
        );
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn the_holder_parent_must_be_a_private_real_directory() {
        use std::os::unix::fs::PermissionsExt;
        let env = Env::new();
        let rt = env.dir.path();
        ensure_private_parent(rt).unwrap();
        let parent = rt.join("claude-cu-esc");
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o750)).unwrap();
        ensure_private_parent(rt).unwrap();
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let other = tempfile::tempdir().unwrap();
        let rt2 = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), rt2.path().join("claude-cu-esc")).unwrap();
        let err = ensure_private_parent(rt2.path()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("symlink"), "{err}");

        let rt3 = tempfile::tempdir().unwrap();
        std::fs::write(rt3.path().join("claude-cu-esc"), "").unwrap();
        assert!(ensure_private_parent(rt3.path()).is_err());
    }
}
