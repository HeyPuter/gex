#!/bin/sh
# The source tree the session image is made from: Debian, plus the programs the
# compositor demo offers.
#
#   web/build-session-tree.sh [work-dir]
#
# `build-graphics-rootfs.sh` takes a tree and turns it into an image. This says
# what is in the tree, which until now was whatever the invocation that happened
# to build it named -- and that turned out to be a Debian with no `apt` in it,
# and with coreutils present but left out of the image. Both are here now, and
# so is the list, so a program joins the session by being named here rather than
# by being installed into someone's /tmp.
#
# The .debs are cached in the work directory, so a second run with one more
# package fetches one more package.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${1:-${TMPDIR:-/tmp}/gex-x}"

# A Debian, the same closure `build-debian.sh` builds on its own: a shell, the
# tools a prompt is worth having, dpkg, and apt -- which is the one that makes
# the image something a person can add to from inside the guest.
BASE="apt dash bash coreutils dpkg findutils grep sed tar gzip diffutils
      debianutils base-files base-passwd hostname ca-certificates
      ncurses-base ncurses-bin less procps usr-is-merged
      libc-bin perl-base debian-archive-keyring"

# And the session: an X server and a Wayland one, the toolkits, the programs the
# picker offers, and Mesa's own GL utilities. `dbus` and `dbus-user-session` are
# not decoration -- KF5 asks for a session bus before it draws anything.
# No `xwayland`: the X server is the one in the page now (see
# `web/build-xwayland-wasm.sh`), cross-built to wasm rather than run as a
# guest program. `xvfb` stays because the graphics-in-browser test starts one.
APPS="xvfb x11-apps xterm twm tk8.6
      mesa-utils glmark2-x11 gtk-3-examples
      kcalc dolphin qt6ct compton-conf explosive-c4
      qt6-wayland qtwayland5 dbus dbus-user-session
      gawk libattr1 libpam-modules-bin libsigsegv2"

echo "the session tree: $(printf '%s' "$BASE $APPS" | wc -w) package roots"
GEX_DEBIAN_ROOTS="$BASE $APPS" exec "$HERE/build-debian.sh" \
    "$WORK/session-tree.tar" "$WORK"
