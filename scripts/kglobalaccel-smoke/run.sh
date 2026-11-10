#!/usr/bin/env bash
# Smoke test of `kwin-portal-bridge abort-watch` against the REAL kglobalaccel
# daemon code (libKGlobalAccelD, which KWin embeds), on a private dbus-daemon
# with private XDG dirs. It never touches your session bus or your
# kglobalshortcutsrc. host.cpp feeds Escape presses into the daemon the way
# KWin does (KGlobalAccelD::keyEvent) and reports whether it consumed them.
#
# Needs: g++, Qt6 Core/DBus/Gui + KF6GlobalAccel headers, python3, busctl,
# dbus-daemon, and kglobalacceld (Plasma 6) - either installed (KGAD_ROOT=/)
# or an extracted package, e.g. on Arch:
#   curl -LO https://geo.mirror.pkgbuild.com/extra/os/x86_64/kglobalacceld-<ver>-x86_64.pkg.tar.zst
#   mkdir root && tar -C root -xf kglobalacceld-*.pkg.tar.zst && KGAD_ROOT=$PWD/root ./run.sh
#
# usage: [KGAD_ROOT=/] scripts/kglobalaccel-smoke/run.sh [bridge binary]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-$here/../../target/debug/kwin-portal-bridge}
root=${KGAD_ROOT:-/}
lib=$(ls "$root"/usr/lib/libKGlobalAccelD.so.* 2>/dev/null | head -1)
[ -n "$lib" ] || { echo "libKGlobalAccelD not found under $root/usr/lib (set KGAD_ROOT)"; exit 2; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
ln -s "$lib" "$work/libKGlobalAccelD.so.0"
g++ -std=c++20 -fPIC -O1 "$here/host.cpp" -o "$work/kgad-host" \
  -I/usr/include/qt6 -I/usr/include/qt6/QtCore -I/usr/include/qt6/QtDBus -I/usr/include/qt6/QtGui \
  -I/usr/include/KF6/KGlobalAccel -I"$root/usr/include/KGlobalAccelD" \
  -L"$work" -l:libKGlobalAccelD.so.0 -lQt6Core -lQt6DBus -lQt6Gui -lKF6GlobalAccel -Wl,-rpath,"$work"
SMOKE_TMP=$work python3 -I -u "$here/smoke.py" "$bin" "$work/kgad-host"
