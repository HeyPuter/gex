#!/bin/sh
# The root filesystem the browser's glxgears demo runs in.
#
#   web/build-graphics-rootfs.sh [out.tar] [source-rootfs]
#
# Debian's own aarch64 binaries: `glxgears` and `Xvfb` as shipped, against
# Debian's own Mesa. Nothing here is built for the occasion, and nothing is
# patched -- the guest runs a real graphics driver and a real X server, and
# the only thing that is not Debian's is what sits on the far side of the
# vtest socket.
#
# The source tree is the one `web/build-session-tree.sh` builds and
# `tests/opengl-in-guest.sh` uses: a Debian with the session's programs
# unpacked into it. What comes across is that tree less its documentation, its
# translations and what only a compiler reads -- so the image is a Debian
# userspace, with `ls`, `bash`, `dpkg` and `apt` in it, and a session can add
# to itself from inside the guest.
#
# It did not use to be. This took the closure a traced run opened, which is
# smaller and is still what `GEX_SESSION_BASE=0` builds, but a curated list
# only ever holds what someone thought to name: a terminal in that image had
# no `ls`.
#
# # No Mesa in the guest
#
# The guest used to run Debian's own Mesa: its megadriver (`swrast_dri.so`,
# every Gallium driver in one 24 MB object), the 106 MB of LLVM llvmpipe
# wants, and the 22 MB of libz3 behind that. All of it is gone, because the
# guest no longer has a graphics driver at all -- its `libGL`/`libEGL` are
# marshallers, and the Mesa that runs is a wasm one in the page's compositor
# worker (see `web/build-mesa-virgl-wasm.sh`).
#
# `Xwayland` is gone for the same reason: the X server is in the page too.
# What stays is the X *clients* and their libraries, which is what a session
# actually runs.
#
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
# The guest architecture. `GEX_ARCH=x86` curates the x86-64 tree (the one
# `GEX_DEBIAN_ARCH=amd64 build-debian.sh` with the graphics roots produces)
# into `rootfs/x86/graphics.tar`, with Debian's x86-64 multiarch names, the
# x86-64 dynamic linker and the x86-64 gexc for the pixbuf cache. Generated
# scripts inside the image are rewritten to the same names at the end.
if [ "${GEX_ARCH:-}" = "x86" ]; then
    MA=x86_64-linux-gnu
    LDSO=ld-linux-x86-64.so.2
    OUT="${1:-$HERE/public/rootfs/x86/graphics.tar}"
    SRC="${2:-/tmp/gex-gfx-amd64/root}"
    WORK="${GEX_ASSET_WORK:-${TMPDIR:-/tmp}/gex-assets}/graphics-x86"
    CC="${GEX_CC:-$(command -v gcc)}"
    GEXC="${GEXC:-$HERE/../target/x86/release/gexc}"
else
    MA=aarch64-linux-gnu
    LDSO=ld-linux-aarch64.so.1
    OUT="${1:-$HERE/public/rootfs/graphics.tar}"
    SRC="${2:-/tmp/gex-x/root}"
    WORK="${GEX_ASSET_WORK:-${TMPDIR:-/tmp}/gex-assets}/graphics"
    TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
    CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"
    fi
mkdir -p "$(dirname "$OUT")"

if [ ! -x "$SRC/usr/bin/glxgears.${MA}" ] || [ ! -x "$SRC/usr/bin/Xvfb" ]; then
    echo "skip: no graphics rootfs at $SRC (wants Mesa, Xvfb and mesa-utils)"
    exit 0
fi
[ -x "$CC" ] || { echo "skip: no cross compiler at $CC"; exit 0; }

rm -rf "$WORK"
mkdir -p "$WORK"
R="$WORK/root"
mkdir -p "$R"

# Copy a list of paths into the image, once per file.
#
# `cp -L` on a list that names both `libfoo.so.1` and `libfoo.so.1.2.3` -- and a
# NEEDED closure does, because both exist and one is a link -- writes the file
# twice. That put two 106 MB copies of libLLVM in the image and two 30 MB copies
# of libicudata, which is most of a quarter of a gigabyte for nothing. So: the
# first name of a file is the file, and every later name of it becomes a
# relative symlink, which is what the source tree has and what ld.so follows.
install_paths() {
    python3 - "$SRC" "$R" "$1" "$WORK/installed" <<'PYEOF'
import os, shutil, sys

src, dst, listing, index = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]


def source_of(rel):
    """Where the source tree keeps this file.

    A name under /usr/lib may live in /lib in the source: they are the same
    directory on a merged system and the image uses the one the guest's
    search path names.
    """
    direct = os.path.join(src, rel)
    if os.path.exists(direct):
        return direct
    if rel.startswith('usr/lib/'):
        alt = os.path.join(src, rel[4:])
        if os.path.exists(alt):
            return alt
    return None

# The index is shared by every pass: the curated list, the NEEDED closure and
# each manifest all name some of the same files, and two 106 MB copies of
# libLLVM is what happens when each pass dedupes only against itself.
#
# Keyed by the file rather than by the path, and a file is a device and an
# inode: a symlink and its target resolve to one name, but so do two *hard*
# links, which is what Debian's DRI directory is -- forty-three names for one
# 24 MB megadriver. Keyed by path that directory is a gigabyte.
by_real = {}
if os.path.exists(index):
    for line in open(index):
        real, rel = line.rstrip('\n').split('\t', 1)
        by_real[real] = rel
files = 0
links = 0
missing = 0
bytes_copied = 0
for line in open(listing):
    rel = line.strip().lstrip('/')
    if not rel or rel.startswith('#'):
        continue
    s = source_of(rel)
    if s is None:
        missing += 1
        continue
    d = os.path.join(dst, rel)
    os.makedirs(os.path.dirname(d), exist_ok=True)
    real = os.path.realpath(s)
    try:
        st = os.stat(s)
    except OSError:
        missing += 1
        continue
    key = f'{st.st_dev}:{st.st_ino}'
    first = by_real.get(key)
    if first is None:
        by_real[key] = rel
        if os.path.isdir(s):
            continue
        shutil.copyfile(real, d)
        shutil.copymode(real, d)
        files += 1
        bytes_copied += os.path.getsize(real)
    else:
        # A second name for a file already installed: a link to it, relative so
        # the image can be unpacked anywhere.
        if first == rel:
            # The same path named by two passes. Not a second name for the
            # file -- it IS the file -- and linking it to itself is what put
            # 258 `gconv/IBM855.so -> IBM855.so` in the image.
            continue
        if os.path.lexists(d):
            continue
        if not os.path.exists(os.path.join(dst, first)):
            # The name it would point at was dropped on purpose (see DROP
            # below). A dangling link is worse than an absent file: `dlopen`
            # of one reports a missing library, which is what was wanted, but
            # every tool that walks the tree trips over it first.
            continue
        target = os.path.relpath(os.path.join(dst, first), os.path.dirname(d))
        os.symlink(target, d)
        links += 1
with open(index, 'w') as f:
    for real, rel in by_real.items():
        f.write(f'{real}\t{rel}\n')
print(f'  {files} files ({bytes_copied / (1 << 20):.0f} MiB), {links} aliased, '
      f'{missing} the source tree does not have', file=sys.stderr)
PYEOF
}

# What to copy. The library list is the closure of a traced run, less LLVM and
# z3; the programs are named because an execve does not look like an open.
LIBS="
libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1 libgcc_s.so.1
libexpat.so.1 libz.so.1 libzstd.so.1 liblzma.so.5 libbz2.so.1.0 liblz4.so.1
libselinux.so.1 libpcre2-8.so.0 libcap.so.2 libcap-ng.so.0 libaudit.so.1
libtinfo.so.6 libgpg-error.so.0 libgcrypt.so.20 libsystemd.so.0
libGL.so.1 libGLX.so.0 libGLX_mesa.so.0 libGLdispatch.so.0 libglapi.so.0
libdrm.so.2 libdrm_amdgpu.so.1 libdrm_nouveau.so.2 libdrm_radeon.so.1
libelf.so.1 libsensors.so.5 libstdc++.so.6
libX11.so.6 libX11-xcb.so.1 libXau.so.6 libXdmcp.so.6 libXext.so.6
libXfixes.so.3 libXfont2.so.2 libXxf86vm.so.1 libxkbfile.so.1
libxcb.so.1 libxcb-dri2.so.0 libxcb-dri3.so.0 libxcb-glx.so.0
libxcb-present.so.0 libxcb-randr.so.0 libxcb-shm.so.0 libxcb-sync.so.1
libxcb-xfixes.so.0 libxshmfence.so.1
libbsd.so.0 libmd.so.0 libedit.so.2 libffi.so.8 libunwind.so.8
libfreetype.so.6 libfontenc.so.1 libpng16.so.16 libpixman-1.so.0
libbrotlicommon.so.1 libbrotlidec.so.1 libxml2.so.2
libicuuc.so.72 libicudata.so.72
libEGL.so.1 libEGL_mesa.so.0 libGLESv2.so.2 libgbm.so.1
libwayland-client.so.0 libwayland-egl.so.1 libwayland-server.so.0
libxkbcommon.so.0 libwayland-cursor.so.0
libxcvt.so.0 libepoxy.so.0 libtirpc.so.3
libgssapi_krb5.so.2 libkrb5.so.3 libk5crypto.so.3 libkrb5support.so.0
libcom_err.so.2 libkeyutils.so.1 libresolv.so.2
libXi.so.6 libXmu.so.6 libXt.so.6 libXrender.so.1 libxcb-damage.so.0
libSM.so.6 libICE.so.6 libuuid.so.1
libXft.so.2 libfontconfig.so.1 libXaw.so.7 libXpm.so.4 libXinerama.so.1
libutempter.so.0 libtcl8.6.so libtk8.6.so libXss.so.1 libjpeg.so.62
libatk-1.0.so.0 libatk-bridge-2.0.so.0 libatspi.so.0 libavahi-client.so.3
libavahi-common.so.3 libblkid.so.1 libcairo-gobject.so.2 libcairo.so.2
libcolordprivate.so.2 libcolord.so.2 libcups.so.2 libdatrie.so.1 libdbus-1.so.3
libdeflate.so.0 libfribidi.so.0 libgdk-3.so.0 libgdk_pixbuf-2.0.so.0
libgio-2.0.so.0 libglib-2.0.so.0 libgmodule-2.0.so.0 libgmp.so.10
libgnutls.so.30 libgobject-2.0.so.0 libgraphite2.so.3 libgthread-2.0.so.0
libgtk-3.so.0 libharfbuzz.so.0 libhogweed.so.6 libidn2.so.0 libjbig.so.0
liblcms2.so.2 libLerc.so.4 libmount.so.1 libnettle.so.8
libp11-kit.so.0 libpango-1.0.so.0 libpangocairo-1.0.so.0 libpangoft2-1.0.so.0
libtasn1.so.6 libthai.so.0 libtiff.so.6 libudev.so.1 libunistring.so.2
libwebp.so.7 libXaw7.so.7 libxcb-render.so.0 libxcb-shape.so.0
libXcomposite.so.1 libXcursor.so.1 libXdamage.so.1 libXrandr.so.2
"
# The GTK3 stack is in the image, and so are Qt 5, Qt 6 and KF5 -- but their
# libraries are not listed here. Hand-listing SONAMEs stopped scaling at the
# first toolkit; the closure pass further down takes them from the programs
# themselves. What is named here is what a *trace* found and DT_NEEDED cannot:
# the X server, the drivers it dlopens, and the programs an execve reaches.
PROGS="Xvfb xkbcomp glxgears.${MA} glxinfo.${MA}
es2gears_wayland.${MA} eglinfo.${MA} Xwayland
es2gears_x11.${MA} es2tri.${MA} es2_info.${MA}
glxdemo.${MA} glxheads.${MA} xeyes xclock xterm xcalc
wish8.6 twm glmark2 gtk3-demo gdk-pixbuf-query-loaders"

mkdir -p "$R/lib/${MA}" "$R/usr/lib/${MA}/dri" \
         "$R/usr/bin" "$R/bin" "$R/etc" "$R/tmp/.X11-unix" "$R/var/lib/xkb" \
         "$R/usr/share/X11" "$R/usr/lib/xorg" "$R/proc" "$R/dev" "$R/root"

# -- the Debian userspace -------------------------------------------------
#
# The image is a Debian root filesystem that has the session's programs in it,
# rather than a list of programs with the libraries they name. A terminal is
# what notices the difference: `ls` was not in the image and neither was `apt`,
# because nothing in the lists below asks for them, and a curated list only
# ever holds what someone thought to name.
#
# So the whole source tree comes across, less four kinds of thing: what is
# documentation (57 MB) or a translation of it (130 MB), what the passes below
# handle specially (the megadriver and libLLVM), and what only a compiler would
# read. `GEX_SESSION_BASE=0` builds the old curated image, `GEX_KEEP_LOCALE=1`
# keeps the translations.
if [ "${GEX_SESSION_BASE:-1}" != "0" ]; then
    MA="$MA" python3 - "$SRC" "$WORK" <<'PYEOF'
import os, sys

src, work = sys.argv[1], sys.argv[2]
ma = os.environ['MA']

# Pruned by directory, and the walk does not descend into these at all.
skip = {
    'usr/share/doc', 'usr/share/man', 'usr/share/info', 'usr/include',
    # Debian's Mesa drivers: nothing in the guest loads one any more.
    f'usr/lib/{ma}/dri',
    # The kernel's, not the image's.
    'proc', 'sys', 'dev', 'run', 'tmp',
    # A cache of the .debs the tree was unpacked from.
    'var/cache/apt/archives',
}
if not os.environ.get('GEX_KEEP_LOCALE'):
    skip.add('usr/share/locale')
# What only Mesa's software rasteriser wanted, and the X server that is now
# in the page.
skip_prefix = (f'usr/lib/{ma}/libLLVM-',
               f'usr/lib/{ma}/libz3.so',
               'usr/bin/Xwayland')

files, dirs, links = [], [], []
for dirpath, dirnames, filenames in os.walk(src):
    rel = os.path.relpath(dirpath, src)
    rel = '' if rel == '.' else rel
    # A symlink to a directory is a name, not a tree: `os.walk` will not
    # descend into it and `install_paths` cannot make one, so it is recorded
    # here and remade as a link. /usr/share/perl/5.36 is one of these.
    keep = []
    for d in dirnames:
        p = os.path.join(rel, d) if rel else d
        if p in skip:
            continue
        if os.path.islink(os.path.join(dirpath, d)):
            links.append((p, os.readlink(os.path.join(dirpath, d))))
        else:
            keep.append(d)
            dirs.append(p)
    dirnames[:] = keep
    for name in filenames:
        p = os.path.join(rel, name) if rel else name
        if not p.startswith(skip_prefix):
            files.append('/' + p)

with open(f'{work}/base', 'w') as f:
    f.write('\n'.join(sorted(files)) + '\n')
# Every directory, so the empty ones survive too: apt wants
# /var/lib/apt/lists to exist and will not create it.
with open(f'{work}/base.dirs', 'w') as f:
    f.write('\n'.join(sorted(dirs)) + '\n')
with open(f'{work}/base.links', 'w') as f:
    for p, target in sorted(links):
        f.write(f'{p}\t{target}\n')
print(f'  {len(files)} files, {len(dirs)} directories, {len(links)} linked')
PYEOF
    while IFS= read -r d; do
        [ -n "$d" ] && mkdir -p "$R/$d"
    done < "$WORK/base.dirs"
    echo "  the Debian userspace:"
    install_paths "$WORK/base"
    while IFS="$(printf '\t')" read -r path target; do
        [ -n "$path" ] || continue
        rm -rf "$R/$path"
        ln -s "$target" "$R/$path"
    done < "$WORK/base.links"
fi

# -L, so a name that is a symlink becomes the file. ld.so opens libraries by
# the SONAME, which is the link name, so the target's own name is never
# needed and copying both would double 22 MB of megadriver.
: > "$WORK/curated"
for l in $LIBS; do
    found=""
    for d in /usr/lib/${MA} /lib/${MA}; do
        [ -e "$SRC$d/$l" ] || continue
        # Always under the multiarch name the SONAME uses, whichever directory
        # the source keeps it in: /lib and /usr/lib are one place on this
        # Debian, and the guest's search path points at one of them.
        echo "/usr/lib/${MA}/$l" >> "$WORK/curated"
        found=1
        break
    done
    [ -n "$found" ] || echo "  missing: $l"
done
echo "  the traced closure:"
install_paths "$WORK/curated"

# The dynamic linker at the path `PT_INTERP` names: `/lib/ld-linux-aarch64.so.1`
# on arm64, `/lib64/ld-linux-x86-64.so.2` on amd64.
if [ "$MA" = "x86_64-linux-gnu" ]; then
    mkdir -p "$R/lib64"
    cp -L "$SRC/lib64/${LDSO}" "$R/lib64/${LDSO}"
else
    cp -L "$SRC/lib/${LDSO}" "$R/lib/${LDSO}"
fi

# /lib and /usr/lib are the same directory on a Debian this recent, and the
# guest's ld.so looks in both. Copying twice would double 22 MB, so the
# search path is what points at one place.
: > "$WORK/progs"
for p in $PROGS; do
    [ -e "$SRC/usr/bin/$p" ] && echo "/usr/bin/$p" >> "$WORK/progs"
done
echo "  the programs:"
install_paths "$WORK/progs"
# A shell, and the two programs the session script cannot do without: `sleep`,
# because dash has no builtin one and the wait for the X socket would spin
# through six hundred iterations in a millisecond and give up before the server
# opened its listener, and `timeout`, because glxgears draws until it is killed
# and a killed process never flushes the five-second report it prints.
#
# Only if they are not there already: the Debian pass above brings all of
# coreutils, and `GEX_SESSION_BASE=0` is the build where it did not run.
[ -e "$R/bin/sh" ] || cp -L "$SRC/bin/dash" "$R/bin/sh"
[ -e "$R/bin/sleep" ] || cp -L "$SRC/bin/sleep" "$R/bin/sleep"
[ -e "$R/usr/bin/timeout" ] || cp -L "$SRC/usr/bin/timeout" "$R/usr/bin/timeout"

# -- the toolkit applications, and everything they link --------------------
#
# The lists above are one traced closure, written down by hand. The Qt, KF5
# and GTK applications the Wayland demo offers reach two hundred libraries
# more between them, and the ones that decide whether an app starts at all
# are in no binary's DT_NEEDED: a toolkit picks its window system from a
# plugin it dlopens, so `QT_QPA_PLATFORM=wayland` is served by
# `plugins/platforms/libqwayland-generic.so`, and that plugin is what pulls
# libQt5WaylandClient in behind it.
#
# So this pass is mechanical rather than curated. Name the programs and the
# plugin directories; take the transitive DT_NEEDED closure of all of it.
# Names resolve the way ld.so resolves them -- the object's own directory
# first, then the multiarch library directories -- and a name that resolves
# to a symlink is copied as the file it points at, because the SONAME is the
# only name anything ever opens.
APPS="${GEX_APPS:-/usr/bin/gtk3-demo /usr/bin/gtk3-widget-factory
      /usr/bin/kcalc /usr/bin/qt6ct /usr/bin/compton-conf /usr/games/explosive-c4
      /usr/bin/dolphin /usr/lib/${MA}/libexec/kf5
      /usr/lib/${MA}/qt5/plugins /usr/lib/${MA}/qt6/plugins
      /usr/lib/${MA}/gdk-pixbuf-2.0 /usr/lib/${MA}/gtk-3.0}"

# shellcheck disable=SC2086
MA="$MA" python3 - "$SRC" $APPS > "$WORK/closure" <<'PYEOF'
import os, struct, sys

root, roots = sys.argv[1], sys.argv[2:]
ma = os.environ['MA']
# Where ld.so looks, after the object's own directory. This Debian keeps its
# libraries in the multiarch directory, and /lib is not a link to /usr/lib
# here, so both are searched -- in the order the loader searches them.
# `pulseaudio/` is libpulse's RUNPATH directory (libpulsecommon lives there).
LIBDIRS = [f'/usr/lib/{ma}', f'/lib/{ma}', '/usr/lib', '/lib', f'/usr/lib/{ma}/pulseaudio']


def needed(path):
    """The DT_NEEDED names of an ELF, read out of its dynamic section.

    Read here rather than shelled out to readelf: this walks a few hundred
    objects, and a process each would be most of the run time.
    """
    try:
        with open(path, 'rb') as f:
            data = f.read()
    except OSError:
        return []
    # 64-bit little-endian only, which is every object in an arm64 rootfs.
    # Anything else in a plugin directory -- a script, a .json -- has no
    # dynamic section to read and contributes nothing.
    if data[:4] != b'\x7fELF' or data[4] != 2:
        return []
    shoff, = struct.unpack_from('<Q', data, 0x28)
    shentsize, shnum = struct.unpack_from('<HH', data, 0x3a)
    sections, dyn = [], None
    for i in range(shnum):
        off = shoff + i * shentsize
        sh_type, = struct.unpack_from('<I', data, off + 4)
        sh_offset, sh_size = struct.unpack_from('<QQ', data, off + 0x18)
        sh_link, = struct.unpack_from('<I', data, off + 0x28)
        sections.append(sh_offset)
        if sh_type == 6:  # SHT_DYNAMIC
            dyn = (sh_offset, sh_size, sh_link)
    if dyn is None:
        return []
    off, size, link = dyn
    strtab = sections[link]
    out = []
    for o in range(off, off + size, 16):
        tag, val = struct.unpack_from('<Qq', data, o)
        if tag == 0:  # DT_NULL
            break
        if tag == 1:  # DT_NEEDED
            name = data[strtab + val:]
            out.append(name[:name.index(b'\0')].decode())
    return out


def resolve(soname, first):
    for d in [first] + LIBDIRS:
        p = os.path.join(root, d.lstrip('/'), soname)
        if os.path.exists(p):
            return '/' + os.path.relpath(p, root)
    return None


seen, missing, work = set(), set(), []
for r in roots:
    p = os.path.join(root, r.lstrip('/'))
    if os.path.isdir(p):
        for dirpath, _, names in os.walk(p):
            work += ['/' + os.path.relpath(os.path.join(dirpath, n), root)
                     for n in names]
    elif os.path.exists(p):
        work.append('/' + r.lstrip('/'))
    else:
        missing.add(r)

while work:
    rel = work.pop()
    if rel in seen:
        continue
    seen.add(rel)
    for so in needed(os.path.join(root, rel.lstrip('/'))):
        r = resolve(so, os.path.dirname(rel))
        if r is None:
            missing.add(so)
        elif r not in seen:
            work.append(r)

for rel in sorted(seen):
    print(rel)
size = sum(os.path.getsize(os.path.realpath(os.path.join(root, r.lstrip('/'))))
           for r in seen)
print(f'  the applications reach {len(seen)} objects, '
      f'{size / (1 << 20):.0f} MiB', file=sys.stderr)
if missing:
    print(f'  missing: {" ".join(sorted(missing))}', file=sys.stderr)
PYEOF

install_paths "$WORK/closure"

# `explosive-c4` lives in /usr/games, which is not on the PATH of the shell
# the page starts. A link, rather than a PATH the page has to know about.
[ -e "$R/usr/games/explosive-c4" ] &&
    ln -sf ../games/explosive-c4 "$R/usr/bin/explosive-c4"

# -- the app deltas recorded in web/rootfs-manifests -----------------------
#
# The closure pass above starts at a program and follows DT_NEEDED. A heavy KDE
# application reaches much more than that: Dolphin runs KIO workers out of
# libexec, loads kcms and parts as kservice plugins, and reads .desktop and
# .kcfg data that names them -- none of it in anyone's DT_NEEDED. That file
# list was derived once (see `web/rootfs-manifests/README.md`) and is written
# down rather than re-derived here, so this pass is a copy of what the
# manifest says.
#
# A path the source tree does not have is counted, not fatal: a manifest may
# name a program this image was not built with, and the count is the signal
# for whether that matters.
for m in "$HERE"/rootfs-manifests/*-delta.txt; do
    [ -f "$m" ] || continue
    echo "  $(basename "$m"):"
    # The manifests were traced on the arm64 image and name its multiarch
    # directory; the x86-64 image wants the same files under its own.
    if [ "$MA" != "aarch64-linux-gnu" ]; then
        sed "s|/aarch64-linux-gnu/|/${MA}/|g" "$m" > "$WORK/$(basename "$m")"
        m="$WORK/$(basename "$m")"
    fi
    install_paths "$m"
done

# Tcl/Tk's script library (wish will not start without init.tcl and tk.tcl) and
# a self-contained demo script. Tk is single-threaded, so unlike GTK it does not
# hit the worker-per-thread futex race.
if [ -d "$SRC/usr/share/tcltk" ]; then
    mkdir -p "$R/usr/share/tcltk"
    cp -r "$SRC/usr/share/tcltk/tcl8.6" "$SRC/usr/share/tcltk/tk8.6" "$R/usr/share/tcltk/"
    [ -f "$SRC/tk-demo.tcl" ] && cp "$SRC/tk-demo.tcl" "$R/tk-demo.tcl"
    ln -sf wish8.6 "$R/usr/bin/wish"
fi

# twm's configuration, for anyone who runs the X session by hand.
#
# The compositor demo does NOT use a window manager in the guest: gex-wayland
# manages the X windows itself (`shims/browser/src/xwm.rs`), which is what
# rootless Xwayland expects and what sway, mutter and kwin all do. twm is
# still in the image, and if it is started it needs this: `RandomPlacement`,
# because its default is to have a user place each window with the mouse, and
# no decorations, because whatever draws the frames should draw all of them.
#
# It has to be a file in the image rather than something a session script
# writes: every process has its own tmpfs, so a `cat > /root/.twmrc` in a
# shell is invisible to the twm it then starts.
mkdir -p "$R/root"
if [ -f "$SRC/root/.twmrc" ]; then
    cp "$SRC/root/.twmrc" "$R/root/.twmrc"
else
    cat > "$R/root/.twmrc" <<'TWMRC'
NoGrabServer
NoTitle
NoHighlight
NoMenus
BorderWidth 0
RandomPlacement
NoRaiseOnMove
OpaqueMove
TWMRC
fi
[ -f "$SRC/session.sh" ] && cp "$SRC/session.sh" "$R/session.sh"

# The terminal's shell.
#
# `xterm` with no `-e` goes looking for a shell in the password file, and this
# image has no entry to find -- so the session starts it as `xterm -e /bin/sh
# /usr/bin/shell.sh` and this is that script. A file in the image for the same
# reason as .twmrc above: a shell cannot write one where another process will
# see it.
mkdir -p "$R/usr/bin"
cat > "$R/usr/bin/shell.sh" <<'SHELL'
#!/bin/sh
# The shell xterm runs: bash if the image has one, because it reads
# /root/.profile for a prompt and a PATH and gives the terminal line editing
# and history. dash otherwise, which is what a curated image has.
[ -x /bin/bash ] && exec /bin/bash -l
exec /bin/sh
SHELL
chmod 755 "$R/usr/bin/shell.sh"

# glmark2's benchmark data: the 3D models, the GLSL shaders, and the textures
# its scenes load. A real GL workload -- shaders, VBOs, framebuffers -- to
# push virglrenderer well past spinning gears.
[ -d "$SRC/usr/share/glmark2" ] && { mkdir -p "$R/usr/share"; cp -r "$SRC/usr/share/glmark2" "$R/usr/share/glmark2"; }

# GLVND's EGL vendor config: Debian's libEGL.so.1 is the vendor-neutral
# dispatch, and without this JSON it never loads libEGL_mesa.so.0 -- eglQuery
# returns no client extensions and eglInitialize fails before any driver. This
# one file is the difference between "no EGL" and Mesa.
[ -d "$SRC/usr/share/glvnd" ] && cp -r "$SRC/usr/share/glvnd" "$R/usr/share/glvnd"
cp -r "$SRC/usr/share/X11/xkb" "$R/usr/share/X11/xkb"
# Fonts and fontconfig, for the toolkit clients (xclock, xterm) that render text
# through Xft. DejaVu is enough for a terminal and a clock; fontconfig scans the
# directory at startup (no prebuilt cache) and writes its own to the cache dir.
if [ -d "$SRC/usr/share/fonts" ]; then
    mkdir -p "$R/usr/share/fonts" "$R/etc" "$R/var/cache/fontconfig"
    cp -r "$SRC/usr/share/fonts" "$R/usr/share/fonts"
    [ -d "$SRC/etc/fonts" ] && cp -r "$SRC/etc/fonts" "$R/etc/fonts"
fi
# Athena-widget apps (xcalc) read their button layout from an app-defaults
# resource file; without it xcalc comes up an empty window.
if [ -d "$SRC/etc/X11/app-defaults" ]; then
    mkdir -p "$R/etc/X11/app-defaults"
    cp -r "$SRC/etc/X11/app-defaults/." "$R/etc/X11/app-defaults/"
fi
# The runtime data the toolkits read, none of which is reachable through
# DT_NEEDED and each of which fails as something else: GTK aborts on a
# GSettings schema it cannot find, a KF5 program comes up with no defaults
# without its .kcfg, and gdk-pixbuf answers "couldn't recognize the image
# file format" for a missing mime database.
for d in usr/share/glib-2.0/schemas usr/share/mime usr/share/icons \
         usr/share/kf5 usr/share/config.kcfg usr/share/kglobalaccel \
         usr/share/kconf_update usr/share/kservices5 usr/share/qt6ct \
         usr/share/compton-conf usr/share/gtk-3.0 usr/share/libthai \
         usr/share/dolphin usr/share/kxmlgui5 usr/share/knotifications5 \
         usr/share/kio usr/share/solid usr/share/kservicetypes5 \
         etc/gtk-3.0 etc/xdg; do
    [ -e "$SRC/$d" ] || continue
    mkdir -p "$R/$(dirname "$d")"
    cp -r "$SRC/$d" "$R/$d"
done

# And the three caches a real system generates in a maintainer script, which
# nothing here runs. The first two are host tools over guest data files --
# both formats are byte-order-tagged rather than native, so the host's own
# copy produces what the guest will read.
if [ -d "$R/usr/share/glib-2.0/schemas" ]; then
    glib-compile-schemas "$R/usr/share/glib-2.0/schemas" ||
        echo "  no compiled schemas -- a GTK client will abort on the first one"
fi
if [ -d "$R/usr/share/mime" ]; then
    update-mime-database "$R/usr/share/mime" ||
        echo "  no mime cache -- gdk-pixbuf will recognise no image format"
fi

# The pixbuf loader cache is the one that cannot be faked. It names absolute
# loader paths that must match the guest's layout, and a hand-edited or
# path-mismatched cache makes gdk-pixbuf free() a bad pointer at parse time.
# The guest cannot build it at startup either -- the forked query-loaders
# writes into a per-process tmpfs the GTK client never sees.
#
# So it is generated by running the guest's OWN query-loaders, under gex, on
# this rootfs. That is the whole point of the project applied to its own
# build: the paths come out guest-absolute because to that program they ARE
# its paths, and the qemu-user this used to need (with a host prefix to strip
# back off afterwards) is gone.
GEXC="${GEXC:-$HERE/../target/release/gexc}"
QL=/usr/lib/${MA}/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders
if [ -x "$GEXC" ] && [ -e "$R$QL" ]; then
    if "$GEXC" run --sysroot "$R" --uid 0 "$R$QL" \
        > "$WORK/loaders.cache" 2>/dev/null &&
        [ -s "$WORK/loaders.cache" ]; then
        cp "$WORK/loaders.cache" \
            "$R/usr/lib/${MA}/gdk-pixbuf-2.0/2.10.0/loaders.cache"
        echo "  pixbuf loaders: $(grep -c '^"/usr' "$WORK/loaders.cache") of them"
    else
        echo "  no pixbuf loader cache -- gdk-pixbuf will load no images"
    fi
fi

[ -e "$SRC/usr/share/drirc.d" ] && cp -r "$SRC/usr/share/drirc.d" "$R/usr/share/drirc.d"
[ -e "$SRC/usr/lib/xorg/protocol.txt" ] && cp "$SRC/usr/lib/xorg/protocol.txt" "$R/usr/lib/xorg/"


# What the image does NOT need, however it got in -- and this runs after
# every pass that puts something in, because it used to run in the middle:
# the manifests below named files it had just deleted, and `install_paths`
# wrote a link to each of them instead.
#
# A NEEDED closure follows everything a program *might* load, and Dolphin's
# reaches the video and audio codecs behind its thumbnailers plus glibc's whole
# charset module set. None of it is on the path of anything this session shows,
# and it is a hundred megabytes of a page's memory before a window appears.
# Dolphin without them thumbnails less; the alternative is Dolphin not
# starting, because the tab ran out of memory unpacking the image.
DROP="${GEX_KEEP_EXTRAS:+}"
if [ -z "${GEX_KEEP_EXTRAS:-}" ]; then
    DROP="libavcodec.so.* libavformat.so.* libavutil.so.* libswscale.so.*
          libswresample.so.* libcodec2.so.* libavfilter.so.* libpostproc.so.*
          libx264.so.* libx265.so.* libvpx.so.* libaom.so.* libdav1d.so.*
          librav1e.so.* libsvtav1enc.so.* libzvbi.so.* libsrt-gnutls.so.*
          libopenh264.so.* libmfx.so.* libva.so.* libva-drm.so.* libva-x11.so.*
          libvidstab.so.* libshine.so.* libtwolame.so.* libmp3lame.so.*
          libopenmpt.so.* libchromaprint.so.* libbs2b.so.* librubberband.so.*
          libLLVM-*.so* libz3.so*"
fi
for pattern in $DROP; do
    rm -f "$R"/usr/lib/${MA}/$pattern
done
# Mesa's drivers and the X server, whichever pass put them here: the guest has
# no graphics driver and does not run an X server, and a manifest written when
# it did still names them.
if [ -z "${GEX_KEEP_EXTRAS:-}" ]; then
    rm -rf "$R/usr/lib/${MA}/dri" "$R/usr/bin/Xwayland"
fi
# glibc's charset modules: 20 MB of tables for encodings nothing here reads.
# UTF-8 needs no module at all.
[ -z "${GEX_KEEP_EXTRAS:-}" ] && rm -rf "$R/usr/lib/${MA}/gconv"

# Nothing here brings LLVM or the megadriver in any more: the guest has no
# Mesa to want them (see the note at the top). What used to be here was a
# choice between copying 106 MB of libLLVM into the image and generating a
# stub of its 280 symbols, and both are gone with the driver that named them.

# The alternatives a maintainer script would have made. Nothing runs those
# here (see build-debian.sh), so the names they install are missing -- and
# `awk` is one a shell script reaches for without thinking.
[ -e "$R/usr/bin/awk" ] || [ ! -e "$R/usr/bin/gawk" ] || ln -s gawk "$R/usr/bin/awk"
[ -e "$R/usr/bin/pager" ] || [ ! -e "$R/usr/bin/less" ] || ln -s less "$R/usr/bin/pager"

cat > "$R/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
EOF
cat > "$R/etc/group" <<'EOF'
root:x:0:
EOF

# The script the demo runs, kept in the image so what the page does is
# readable from the tar rather than only from the JavaScript.
cat > "$R/gears.sh" <<'EOF'
export DISPLAY=:0
export LD_LIBRARY_PATH=/usr/lib/aarch64-linux-gnu
# Before the X server, not after it.
#
# virpipe is Mesa's own name for virgl over a vtest socket, and it is the only
# thing this guest is ever told about its GPU. It used to be exported further
# down, just before the client -- which left *Xvfb* to pick a driver for its
# own GLX, and it picked llvmpipe. The heartbeat caught it: at six and ten
# seconds the guest was executing at 0x6501bd00 and 0x668fbbfc, deep in the
# 106 MB of libLLVM, before the X socket had even appeared.
#
# Nothing in this demo wants a software rasteriser. The renderer is on the
# other side of the socket.
export GALLIUM_DRIVER=virpipe
# Mesa's on-disk shader cache runs on its own thread. Under the worker-per-thread
# model that thread is a second Web Worker, and its handshake with the main one
# rides a cross-worker futex wake that the browser scheduler can drop -- the
# process then deadlocks in futex after the fbconfig probe, before it ever opens
# the rendering connection (glxgears/es2gears both hung here). No cache, no
# thread, no lost wake.
export MESA_SHADER_CACHE_DISABLE=true
export MESA_GLSL_CACHE_DISABLE=1
# Every step says so. In a page this is the only channel there is, and a run
# that is merely slow looks exactly like one that is stuck without it -- the
# first browser attempt at this printed nothing for half an hour and the
# question "where is it" had no answer at all.
echo "gex: starting the X server"
# The screen size is a variable so that a run can ask the question "is the
# frame rate bound by pixels or by calls?" without rebuilding this image.
# glxgears with no geometry fills the screen, so this is the window too.
Xvfb :0 -screen 0 "${GEARS_SIZE:-300x300}x24" -nolisten tcp &
i=0
while [ ! -e /tmp/.X11-unix/X0 ]; do
  i=$((i + 1))
  [ "$i" -gt 600 ] && { echo "gex: no X socket after $i tries"; exit 1; }
  sleep 1
done
echo "gex: X server up after $i seconds"
if [ -e /tmp/.virgl_test ]; then
  echo "gex: the renderer is listening"
else
  echo "gex: no /tmp/.virgl_test -- Mesa will connect to nothing and wait"
fi
echo "gex: loading Mesa and drawing"
# Unbounded unless asked otherwise, and whoever started it decides when to
# stop -- a page closes the tab, a harness gives up. glxgears reports its
# frame rate every five seconds, which is the signal either of them wants.
#
# `timeout` used to bound this and was removed: it forks the program and
# waits, and `wait` came back ECHILD in a browser, so the run ended with
# `timeout: error waiting for command: No child processes` and nothing to say
# whether the gears had drawn. That is a real defect with its own task, but it
# is not this demo's, and having it in the way made every run ambiguous.
if [ -n "${GEARS_SECONDS:-}" ] && [ "${GEARS_SECONDS}" != "0" ]; then
  timeout "$GEARS_SECONDS" glxgears.aarch64-linux-gnu
  s=$?
  # 124 is `timeout` saying it stopped the program, which for a run given a
  # budget is how it is *supposed* to end. It used to be `exec`ed and passed
  # straight out, so a demo that drew for ninety seconds at thirty frames a
  # second reported "exit 124" and coloured itself a failure.
  [ "$s" = 124 ] && s=0
  exit "$s"
fi
exec glxgears.aarch64-linux-gnu
EOF
chmod +x "$R/gears.sh"

mkdir -p "$(dirname "$OUT")"
# The scripts baked into the image name the multiarch directory and the
# mesa-utils program suffix; for the x86-64 image, say the x86-64 names.
if [ "$MA" != "${MA}" ]; then
    for f in "$R/gears.sh" "$R/session.sh" "$R/tk-demo.tcl" "$R/root/.twmrc"; do
        [ -f "$f" ] && sed -i "s/${MA}/$MA/g" "$f"
    done
fi
tar -C "$R" -cf "$OUT" .
echo "  $OUT: $(du -h "$OUT" | cut -f1)"

# -- the session image -----------------------------------------------------
#
# The compositor demo runs several of these programs at once, so what it costs
# a browser tab to unpack is a real constraint: an image is fetched and then
# written into the guest's filesystem, so every megabyte is paid for twice.
#
# libLLVM is 107 MB of that, and the temptation is to stub it for this image --
# nothing in the session should enter llvmpipe, since Xwayland's glamor finds
# no GPU and falls back to its own software blitting while the GL clients render
# through Mesa's virgl driver. It does not work, and it fails quietly: with a
# stub of `brk #0` in its place Xwayland initialises every extension, GLX
# included, says it is ready, and then never answers a client. Its X clients and
# the compositor's own window manager both sit in `connect` forever. Something
# on the way through swrast's DRI entry points calls in, exactly as it does for
# Xvfb.
#
# So this image is the graphics image, and what it adds is SuperTuxKart, because
# a session that offers it needs one filesystem. `tar` archives are streams, so
# appending is a copy rather than an unpack, and a path in both resolves the way
# an extractor resolves it: the later entry wins.
STK="$(dirname "$OUT")/stk.tar"
SESSION="$(dirname "$OUT")/session.tar"
if [ "${GEX_SESSION_TAR:-1}" != "0" ]; then
    cp "$OUT" "$SESSION"
    echo "  $SESSION: $(du -h "$SESSION" | cut -f1)"
    if [ -f "$STK" ]; then
        WITH_STK="$(dirname "$OUT")/session-stk.tar"
        cp "$SESSION" "$WITH_STK"
        tar --concatenate --file="$WITH_STK" "$STK"
        echo "  $WITH_STK: $(du -h "$WITH_STK" | cut -f1) (with supertuxkart)"
    fi
fi
