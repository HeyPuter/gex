#!/bin/bash
# 32: the page's bundle, $W7_OUT/shell.tar, from the tree $W7_OUT/bundle
# (kept for 33):
#   pfx/   the shell's prefix (31), with dll-bases (dll-bases.py: the DLLs
#          the recording w3-session.json names, pinned from 0x60000000)
#   w3/    the dist (16), links dereferenced
#   usr/   share/X11/xkb (the host's xkb-data: Wine's Wayland driver's
#          keymaps) and share/zoneinfo (the host's tzdata, less right/ and
#          posix/: the page passes the browser's zone as TZ=:Area/City)
. "$(dirname "$0")/lib.sh"
PFX=$W7_OUT/prefix; DIST=$W7_OUT/dist; B=$W7_OUT/bundle; TAR=$W7_OUT/shell.tar
[ -f "$PFX/drive_c/winlogon.exe" ] || { echo "run 31-stage.sh first" >&2; exit 2; }
[ -d /usr/share/X11/xkb ] && [ -d /usr/share/zoneinfo ] || { echo "needs the host's xkb-data and tzdata (/usr/share/X11/xkb, /usr/share/zoneinfo)" >&2; exit 2; }
step "the tree"
chmod -R u+w "$B" 2>/dev/null || true; rm -rf "$B"; mkdir -p "$B/w3" "$B/usr/share/X11"
cp -a "$PFX" "$B/pfx"
cp -aL "$DIST/." "$B/w3/"
cp -a /usr/share/X11/xkb "$B/usr/share/X11/"
tar -C /usr/share --exclude=zoneinfo/right --exclude=zoneinfo/posix -cf - zoneinfo | tar -C "$B/usr/share" -xf -
step "dll-bases"
python3 -I "$W7_TOOLS/dll-bases.py" "$B" "$W7_TOOLS/w3-session.json" "$B/pfx/dll-bases" "$W7_OUT/units.txt"
step "the tar"
rm -f "$TAR"; (cd "$B" && tar -cf "$TAR" pfx w3 usr)
step "done: $TAR ($(du -h "$TAR" | cut -f1))"
