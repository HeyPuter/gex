#!/bin/sh
# Boot a real userspace: BusyBox init as pid 1, from an /etc/inittab.
#
# The offline suite has `init`, a purpose-built pid 1 that exercises the
# process table directly. This is the other half of the claim: an init system
# nobody wrote for gex, doing what it does on a real machine -- parsing an
# inittab, running a sysinit script, starting services in their own sessions,
# adopting what they leave behind, and powering the machine off at the end.
#
# Fetches BusyBox, so it is not part of the offline suite.
#
#   usage: tests/real-programs/busybox-init.sh [work-dir]
set -eu

WORK="${1:-${TMPDIR:-/tmp}/gex-busybox-init}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
GEXC="$ROOT/target/release/gexc"
TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
VERSION=1.36.1

if [ ! -x "$GEXC" ]; then
    echo "skip: build gexc first (cargo build --release)"
    exit 0
fi

mkdir -p "$WORK"
BB="$WORK/busybox"
if [ ! -x "$BB" ]; then
    SRC="$WORK/busybox-$VERSION"
    if [ ! -d "$SRC" ]; then
        echo "fetching busybox $VERSION..."
        curl -fsSL "https://busybox.net/downloads/busybox-$VERSION.tar.bz2" \
            -o "$WORK/busybox.tar.bz2"
        tar -C "$WORK" -xf "$WORK/busybox.tar.bz2"
    fi
    echo "building busybox..."
    make -C "$SRC" defconfig >/dev/null
    sed -i 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/' "$SRC/.config"
    make -C "$SRC" -j"$(nproc)" \
        CROSS_COMPILE="$TOOLCHAIN/bin/aarch64-linux-musl-" >/dev/null 2>&1
    cp "$SRC/busybox" "$BB"
fi

# A root filesystem: the one binary, the applet links an inittab names, and
# the boot scripts themselves.
R="$WORK/root"
rm -rf "$R"
mkdir -p "$R/bin" "$R/etc/init.d" "$R/dev" "$R/proc" "$R/tmp"
cp "$BB" "$R/bin/busybox"
for a in sh echo cat cut ls mount umount sleep init poweroff halt reboot \
         hostname sync uname mkdir printf test true grep ps; do
    ln -sf busybox "$R/bin/$a"
done

cat > "$R/etc/inittab" <<'EOF'
::sysinit:/bin/sh /etc/init.d/rcS
::wait:/bin/sh /etc/init.d/services
::wait:/bin/sleep 2
::wait:/bin/sh -c 'echo "[init] shutting down"; /bin/sync; /bin/poweroff -f'
EOF

cat > "$R/etc/init.d/rcS" <<'EOF'
#!/bin/sh
echo "[rcS] pid=$$ ppid=$PPID"
/bin/mount -t proc proc /proc && echo "[rcS] mounted /proc"
/bin/mount -t tmpfs tmpfs /tmp && echo "[rcS] mounted /tmp"
echo "[rcS] $(/bin/uname -sm)"
EOF

cat > "$R/etc/init.d/services" <<'EOF'
#!/bin/sh
echo "[svc] pid=$$ ppid=$PPID"
# A daemon that outlives the script that started it. Its parent exits as soon
# as this returns, so it can only keep running if init adopts it -- and it
# reports the parent it was given.
/bin/sh -c 'sleep 1; printf "[daemon] adopted by pid "; exec /bin/cut -d" " -f4 /proc/self/stat' &
echo "[svc] started a daemon"
EOF
chmod +x "$R/etc/init.d/rcS" "$R/etc/init.d/services"

# The first run of each applet path translates the whole binary; later ones
# come from the module cache. Warm the ones the inittab reaches, so the boot
# itself is not mostly translation.
echo "warming the module cache..."
for a in busybox sh echo cut sleep mount uname sync poweroff; do
    "$GEXC" run --sysroot "$R" "$R/bin/$a" --help >/dev/null 2>&1 || true
done

echo "booting..."
out="$WORK/boot.log"
if ! "$GEXC" run --sysroot "$R" "$R/bin/busybox" init >"$out" 2>"$WORK/boot.err"; then
    echo "FAIL: init exited $?"
    tail -5 "$WORK/boot.err"
    exit 1
fi
cat "$out"

fail=0
check() {
    if grep -q "$1" "$out"; then
        echo "ok   $2"
    else
        echo "FAIL $2"
        fail=1
    fi
}
check '^\[rcS\] pid=2 ppid=1'      "sysinit runs as init's child"
check '^\[rcS\] mounted /proc'     "the guest can mount /proc itself"
check '^\[rcS\] mounted /tmp'      "and a tmpfs"
check '^\[svc\] pid=[0-9]* ppid=1' "services are started by init"
check '^\[daemon\] adopted by pid 1' "an orphaned daemon is reparented to init"
check '^\[init\] shutting down'    "the inittab runs to the end"
exit $fail
