#!/bin/sh
# The x86-64 busybox rootfs: Debian's own statically linked busybox (the
# `busybox-static` amd64 package) in the same tree layout web/build-assets.sh
# gives the AArch64 one, so the two demos differ only by architecture.
#
#   web/build-busybox-x86.sh            -> web/public/rootfs/x86/busybox.tar
set -eu
cd "$(dirname "$0")"
MIRROR="${GEX_DEBIAN_MIRROR:-http://deb.debian.org/debian}"
SUITE="${GEX_DEBIAN_SUITE:-bookworm}"
WORK="${TMPDIR:-/tmp}/gex-busybox-x86"
PUB="$(pwd)/public"
GEXC="${GEXC:-$(cd .. && pwd)/target/x86/release/gexc}"
mkdir -p "$WORK" "$PUB/rootfs/x86"
if [ ! -f "$WORK/busybox" ]; then
    echo "fetching busybox-static (amd64)..."
    curl -fsSL "$MIRROR/dists/$SUITE/main/binary-amd64/Packages.gz" | gunzip -c \
        | awk '/^Package: busybox-static$/{p=1} p&&/^Filename:/{print $2; exit}' > "$WORK/filename"
    curl -fsSL "$MIRROR/$(cat "$WORK/filename")" -o "$WORK/bb.deb"
    (cd "$WORK" && rm -rf x && mkdir x && cd x && ar x ../bb.deb && tar -xf data.tar.*)
    cp "$WORK"/x/bin/busybox "$WORK/busybox"
fi
BB="$WORK/busybox"
R="$WORK/bbroot"
rm -rf "$R"
mkdir -p "$R/bin" "$R/etc" "$R/root" "$R/tmp" "$R/proc" "$R/dev" "$R/usr/bin"
cp "$BB" "$R/bin/busybox"
# The applet links, from the binary itself (run natively: this is an x86-64
# host, and the answer is the same under gex).
for a in $("$BB" --list 2>/dev/null); do
    case "$a" in
        */*) continue ;;
    esac
    ln -sf busybox "$R/bin/$a"
done
cat > "$R/etc/profile" <<'EOP'
export PS1='\w \$ '
export PATH=/bin:/usr/bin
EOP
printf 'root:x:0:0:root:/root:/bin/sh\n' > "$R/etc/passwd"
printf 'root:x:0:\n' > "$R/etc/group"
cat > "$R/root/hello.c" <<'EOP'
#include <stdio.h>
int main(void) { puts("a file that came with the rootfs"); return 0; }
EOP
tar -C "$R" -cf "$PUB/rootfs/x86/busybox.tar" .
echo "  $PUB/rootfs/x86/busybox.tar: $(du -h "$PUB/rootfs/x86/busybox.tar" | cut -f1)"
# A smoke test of the binary under the x86-64 gexc, when one is built.
if [ -x "$GEXC" ]; then
    "$GEXC" run "$BB" echo "busybox under gex: ok" || echo "  (gexc smoke test failed)"
fi
