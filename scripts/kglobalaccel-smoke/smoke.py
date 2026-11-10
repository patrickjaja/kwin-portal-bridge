"""Real-kglobalacceld smoke test of `kwin-portal-bridge abort-watch`.

Private dbus-daemon + private XDG dirs; never the user's session bus.
Run through run.sh.
usage: smoke.py <bridge binary> <kglobalacceld host binary>
"""
import os, select, signal, subprocess, sys, tempfile, time

# python3 -I ignores the environment for Python itself; SMOKE_TMP still
# comes from os.environ.

BIN, HOST = sys.argv[1], sys.argv[2]
T = tempfile.mkdtemp(prefix="kga-smoke-", dir=os.environ.get("SMOKE_TMP"))
for d in ("cfg", "state", "data", "run", "home"):
    os.makedirs(f"{T}/{d}", mode=0o700)
open(f"{T}/bus.conf", "w").write(f"""<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig><type>session</type><listen>unix:dir={T}</listen><auth>EXTERNAL</auth>
<policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy></busconfig>""")
bus = subprocess.Popen(["dbus-daemon", f"--config-file={T}/bus.conf", "--nofork", "--print-address=1"], stdout=subprocess.PIPE, text=True)
addr = bus.stdout.readline().strip()
assert addr.startswith(f"unix:path={T}/") and addr != os.environ.get("DBUS_SESSION_BUS_ADDRESS"), addr
env = {"PATH": os.environ["PATH"], "DBUS_SESSION_BUS_ADDRESS": addr, "XDG_CONFIG_HOME": f"{T}/cfg",
       "XDG_STATE_HOME": f"{T}/state", "XDG_DATA_HOME": f"{T}/data", "XDG_RUNTIME_DIR": f"{T}/run", "HOME": f"{T}/home"}
host = subprocess.Popen([HOST], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(f"{T}/host.err", "w"), text=True, env=env, bufsize=1)
print("host:", host.stdout.readline().strip())
passed = failed = 0
def check(name, got, want):
    global passed, failed
    ok = got == want
    passed += ok; failed += not ok
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f": got {got!r} want {want!r}"))
def press():
    host.stdin.write("press\n"); host.stdin.flush()
    return host.stdout.readline().strip()
def busctl(*args):
    r = subprocess.run(["busctl", f"--address={addr}", "call", "org.kde.kglobalaccel", *args], capture_output=True, text=True)
    return (r.stdout + r.stderr).strip()
ARMED_RC = ["[claude-cu-abort]", "_k_friendly_name=Claude: stop Computer Use"]
def rc():
    p = f"{T}/cfg/kglobalshortcutsrc"
    txt = open(p).read() if os.path.exists(p) else ""
    return [l for l in txt.splitlines() if "claude" in l.lower() or "Stop Computer" in l]
class Watch:
    def __init__(self):
        self.p = subprocess.Popen([BIN, "abort-watch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=open(f"{T}/watch.err", "a"), text=True, env=env, bufsize=1)
    def line(self, timeout=2.0):
        r, _, _ = select.select([self.p.stdout], [], [], timeout)
        return self.p.stdout.readline().strip() if r else None
    def send(self, cmd):
        self.p.stdin.write(cmd + "\n"); self.p.stdin.flush()
try:
    check("unbound Escape not consumed", press(), "handled=0")
    w = Watch()
    print("ready line:", w.line(5))
    check("physical Escape consumed by the grab", press(), "handled=1")
    check("abort reported", w.line(), '{"event":"abort"}')
    # kglobalaccel writes its settings 500 ms after a change: the component
    # group (friendly name only, never the Escape binding) is on disk while
    # armed.
    time.sleep(0.7)
    check("kglobalshortcutsrc while armed: component group, no key binding", rc(), ARMED_RC)
    print("getComponent while armed:", busctl("/kglobalaccel", "org.kde.KGlobalAccel", "getComponent", "s", "claude-cu-abort"))
    print("shortcutNames:", busctl("/component/claude_cu_abort", "org.kde.kglobalaccel.Component", "shortcutNames"))
    print("allMainComponents:", busctl("/kglobalaccel", "org.kde.KGlobalAccel", "allMainComponents"))
    w.send("suspend"); check("suspend ack", w.line(), '{"event":"ack","cmd":"suspend"}')
    check("Escape while suspended reaches the app", press(), "handled=0")
    check("no abort while suspended", w.line(0.4), None)
    t0 = time.monotonic(); w.send("resume"); ack = w.line()
    print(f"resume ack after {(time.monotonic()-t0)*1000:.0f} ms")
    check("resume ack", ack, '{"event":"ack","cmd":"resume"}')
    check("re-grabbed after resume", press(), "handled=1")
    check("abort after resume", w.line(), '{"event":"abort"}')
    w.p.stdin.close(); check("EOF exits 0", w.p.wait(5), 0)
    check("Escape released after exit", press(), "handled=0")
    check("no claude entry in kglobalshortcutsrc after exit", rc(), [])
    print("getComponent after exit:", busctl("/kglobalaccel", "org.kde.KGlobalAccel", "getComponent", "s", "claude-cu-abort"))

    w = Watch(); w.line(5); w.p.send_signal(signal.SIGTERM)
    check("SIGTERM exits 0", w.p.wait(5), 0)
    check("released after SIGTERM", press(), "handled=0")

    w = Watch(); w.line(5); w.p.kill(); w.p.wait()
    check("stale grab after SIGKILL (known limitation)", press(), "handled=1")
    time.sleep(0.7)
    check("kglobalshortcutsrc after SIGKILL: the group stays (no key binding)", rc(), ARMED_RC)
    w = Watch(); print("re-arm ready:", w.line(5))
    check("re-armed", press(), "handled=1"); check("abort", w.line(), '{"event":"abort"}')
    w.p.stdin.close(); check("exit 0", w.p.wait(5), 0)
    check("released after stale cleanup + exit", press(), "handled=0")
    check("no claude entry at the end", rc(), [])

    # Two holders: one registration, last out removes.
    a = Watch(); a.line(5); b = Watch(); b.line(5)
    check("two holders: grab", press(), "handled=1")
    check("abort to holder A", a.line(), '{"event":"abort"}'); check("abort to holder B", b.line(), '{"event":"abort"}')
    a.p.stdin.close(); a.p.wait(5)
    check("first out keeps the grab", press(), "handled=1"); b.line()
    b.p.stdin.close(); b.p.wait(5)
    check("last out releases", press(), "handled=0")

    print("== conflict: another component owns Escape")
    busctl("/kglobalaccel", "org.kde.KGlobalAccel", "doRegister", "as", "4", "org.example.app", "cancel", "Example", "Cancel")
    print(busctl("/kglobalaccel", "org.kde.KGlobalAccel", "setShortcutKeys", "asa(ai)u", "4", "org.example.app", "cancel", "Example", "Cancel", "1", "4", "16777216", "0", "0", "0", "6"))
    w = Watch(); print("conflict line:", w.line(5)); check("conflict exits 2", w.p.wait(5), 2)
    print("getComponent after conflict:", busctl("/kglobalaccel", "org.kde.KGlobalAccel", "getComponent", "s", "claude-cu-abort"))

    print("== stale grab + a foreign Escape: the stale grab is still removed")
    # (Example: Cancel holds Escape too, so presses are consumed either way:
    # check our component instead.)
    busctl("/kglobalaccel", "org.kde.KGlobalAccel", "unregister", "ss", "org.example.app", "cancel")
    w = Watch(); w.line(5); w.p.kill(); w.p.wait()
    check("stale component after SIGKILL", busctl("/kglobalaccel", "org.kde.KGlobalAccel", "getComponent", "s", "claude-cu-abort"), 'o "/component/claude_cu_abort"')
    busctl("/kglobalaccel", "org.kde.KGlobalAccel", "doRegister", "as", "4", "org.example.app", "cancel", "Example", "Cancel")
    busctl("/kglobalaccel", "org.kde.KGlobalAccel", "setShortcutKeys", "asa(ai)u", "4", "org.example.app", "cancel", "Example", "Cancel", "1", "4", "16777216", "0", "0", "0", "6")
    w = Watch(); print("line:", w.line(5)); check("refused (exit 2)", w.p.wait(5), 2)
    check("stale component removed although arming was refused", busctl("/kglobalaccel", "org.kde.KGlobalAccel", "getComponent", "s", "claude-cu-abort"), "Call failed: The component 'claude-cu-abort' doesn't exist.")
    busctl("/kglobalaccel", "org.kde.KGlobalAccel", "unregister", "ss", "org.example.app", "cancel")
    time.sleep(0.7)
    check("no claude entry after the refused arm", rc(), [])

    print("== conflict: a foreign Esc, X sequence (match type Shadows)")
    busctl("/kglobalaccel", "org.kde.KGlobalAccel", "doRegister", "as", "4", "org.example.vi", "leave", "Vi", "Leave")
    print(busctl("/kglobalaccel", "org.kde.KGlobalAccel", "setShortcutKeys", "asa(ai)u", "4", "org.example.vi", "leave", "Vi", "Leave", "1", "4", "16777216", "88", "0", "0", "6"))
    w = Watch(); line = w.line(5); print("line:", line)
    check("Esc, X refuses (exit 2)", w.p.wait(5), 2)
    check("message names it", "Vi: Leave" in (line or ""), True)
    busctl("/kglobalaccel", "org.kde.KGlobalAccel", "unregister", "ss", "org.example.vi", "leave")

    print("== kglobalaccel goes away while armed")
    w = Watch(); print("ready:", w.line(5))
    host.stdin.write("quit\n"); host.stdin.flush(); host.wait(5)
    print("line:", w.line(5)); check("exit 1 after kglobalaccel left", w.p.wait(5), 1)
finally:
    print("== watcher stderr\n" + open(f"{T}/watch.err").read())
    print("== host stderr (tail)\n" + "".join(open(f"{T}/host.err").readlines()[-10:]))
    for p in (host, bus):
        p.kill()
    print(f"RESULT pass={passed} fail={failed}")
    subprocess.run(["rm", "-rf", T])
sys.exit(1 if failed or passed == 0 else 0)
