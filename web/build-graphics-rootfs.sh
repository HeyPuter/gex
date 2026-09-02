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
# The source tree is the one `tests/opengl-in-guest.sh` uses, which is 342 MB.
# A page cannot ask for that, so this takes the closure a traced run actually
# opens, which is 204 MB, and then removes the two libraries that make up most
# of it.
#
# # LLVM, and the switch that removes it
#
# Debian ships one megadriver: `swrast_dri.so` is every Gallium driver at
# once, and it declares `libLLVM-15.so.1` in DT_NEEDED because *llvmpipe*
# wants it -- 106 MB, plus the 21 MB of libz3 that LLVM in turn wants. That
# is 127 MB of a 208 MB image, and in a browser it is worse than download
# size: a dispatch slot is claimed per 4 KiB of guest text, a browser will
# not grow the table past ten million of them, and llvmpipe is a great deal
# of text.
#
# `GEX_LLVM_STUB=1` replaces it with a stand-in: the 280 symbols the
# megadriver imports from LLVM, each one a `brk #0`. Nothing on the virgl
# path should call them, and anything that does dies at the instruction with
# the guest PC in the trap, which is a far better failure than a quietly
# different picture.
#
# It does not work, and the question is now settled rather than open. Xvfb
# brings up GLX through the swrast DRI entry points and *calls* into llvmpipe,
# not merely maps it: with the renderer listening and `GALLIUM_DRIVER=virpipe`
# exported before the X server, the run still stopped at
# `gex: breakpoint at pc=0x6d435040` five seconds in, before the X socket
# appeared. The switch remains for anyone who wants to try a client-only
# workload, where nothing brings up a software rasteriser -- but it will not
# get an X server up, so it is off.
#
# Which also means the 106 MB is not avoidable this way. `swrast_dri.so`
# names libLLVM in DT_NEEDED, so ld.so maps it at dlopen whichever Gallium
# driver is chosen, and Xvfb writes out 221 MiB every time it is swapped.
# That is what makes the arena split (task #71) the thing to do instead.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
# The guest architecture. `GEX_ARCH=x86` curates the x86-64 tree (the one
# `GEX_DEBIAN_ARCH=amd64 build-debian.sh` with the graphics roots produces)
# into `rootfs/x86/graphics.tar`, with Debian's x86-64 multiarch names, the
# x86-64 dynamic linker, the host compiler for the LLVM stub and the x86-64
# gexc for the pixbuf cache. Generated scripts inside the image are
# rewritten to the same names at the end.
if [ "${GEX_ARCH:-}" = "x86" ]; then
    MA=x86_64-linux-gnu
    LDSO=ld-linux-x86-64.so.2
    TRAP_INSN='ud2'
    OUT="${1:-$HERE/public/rootfs/x86/graphics.tar}"
    SRC="${2:-/tmp/gex-gfx-amd64/root}"
    WORK="${GEX_ASSET_WORK:-${TMPDIR:-/tmp}/gex-assets}/graphics-x86"
    CC="${GEX_CC:-$(command -v gcc)}"
    GEXC="${GEXC:-$HERE/../target/x86/release/gexc}"
else
    MA=${MA}
    LDSO=${LDSO}
    TRAP_INSN='brk #0'
    OUT="${1:-$HERE/public/rootfs/graphics.tar}"
    SRC="${2:-/tmp/gex-x/root}"
    WORK="${GEX_ASSET_WORK:-${TMPDIR:-/tmp}/gex-assets}/graphics"
    TOOLCHAIN="${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}"
    CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"
    fi
mkdir -p "$(dirname "$OUT")"

if [ ! -x "$SRC/usr/bin/glxgears.$MA" ] || [ ! -x "$SRC/usr/bin/Xvfb" ]; then
    echo "skip: no graphics rootfs at $SRC (wants Mesa, Xvfb and mesa-utils)"
    exit 0
fi
[ -x "$CC" ] || { echo "skip: no cross compiler at $CC"; exit 0; }

rm -rf "$WORK"
mkdir -p "$WORK"
R="$WORK/root"
mkdir -p "$R"

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
# The x86-64 swrast_dri.so links libdrm_intel as well (the aarch64 build has
# no Intel DRM); without it the X servers have no software GL renderer and
# glxgears gets "couldn't get an RGB, Double-buffered visual".
[ "${GEX_ARCH:-}" = x86 ] && LIBS="$LIBS libdrm_intel.so.1 libpciaccess.so.0"
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

# -L, so a name that is a symlink becomes the file. ld.so opens libraries by
# the SONAME, which is the link name, so the target's own name is never
# needed and copying both would double 22 MB of megadriver.
for l in $LIBS; do
    for d in "$SRC/usr/lib/${MA}" "$SRC/lib/${MA}"; do
        [ -e "$d/$l" ] || continue
        cp -L "$d/$l" "$R/usr/lib/${MA}/$l"
        break
    done
    [ -e "$R/usr/lib/${MA}/$l" ] || echo "  missing: $l"
done
cp -L "$SRC/usr/lib/${MA}/dri/swrast_dri.so" \
      "$R/usr/lib/${MA}/dri/swrast_dri.so"
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
for p in $PROGS; do
    [ -e "$SRC/usr/bin/$p" ] && cp -L "$SRC/usr/bin/$p" "$R/usr/bin/$p"
done
cp -L "$SRC/bin/dash" "$R/bin/sh"
# The one coreutils program in the image. dash has no `sleep` builtin, and
# without one the wait for the X socket spins through six hundred iterations
# in a millisecond and gives up before Xvfb has opened its listener.
cp -L "$SRC/bin/sleep" "$R/bin/sleep"
# And `timeout`, so a run can be bounded. glxgears has no frame count and no
# way to stop: it draws until it is killed, and a killed process does not
# flush the five-second report that is the only thing it prints.
cp -L "$SRC/usr/bin/timeout" "$R/usr/bin/timeout"

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
      /usr/lib/${MA}/qt5/plugins /usr/lib/${MA}/qt6/plugins
      /usr/lib/${MA}/gdk-pixbuf-2.0 /usr/lib/${MA}/gtk-3.0}"

# shellcheck disable=SC2086
python3 - "$SRC" "$MA" $APPS > "$WORK/closure" <<'PYEOF'
import os, struct, sys

root, ma, roots = sys.argv[1], sys.argv[2], sys.argv[3:]
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

while read -r rel; do
    mkdir -p "$R$(dirname "$rel")"
    cp -L "$SRC$rel" "$R$rel"
done < "$WORK/closure"

# `explosive-c4` lives in /usr/games, which is not on the PATH of the shell
# the page starts. A link, rather than a PATH the page has to know about.
[ -e "$R/usr/games/explosive-c4" ] &&
    ln -sf ../games/explosive-c4 "$R/usr/bin/explosive-c4"

# Tcl/Tk's script library (wish will not start without init.tcl and tk.tcl) and
# a self-contained demo script. Tk is single-threaded, so unlike GTK it does not
# hit the worker-per-thread futex race.
if [ -d "$SRC/usr/share/tcltk" ]; then
    mkdir -p "$R/usr/share/tcltk"
    cp -r "$SRC/usr/share/tcltk/tcl8.6" "$SRC/usr/share/tcltk/tk8.6" "$R/usr/share/tcltk/"
    [ -f "$SRC/tk-demo.tcl" ] && cp "$SRC/tk-demo.tcl" "$R/tk-demo.tcl"
    ln -sf wish8.6 "$R/usr/bin/wish"
fi

# A window-manager session: twm managing several clients at once. twm's config
# (auto-placement, since there is no user to drag windows) and the launcher.
[ -f "$SRC/root/.twmrc" ] && { mkdir -p "$R/root"; cp "$SRC/root/.twmrc" "$R/root/.twmrc"; }
[ -f "$SRC/session.sh" ] && cp "$SRC/session.sh" "$R/session.sh"

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
         etc/gtk-3.0; do
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


if [ -n "${GEX_LLVM_STUB:-}" ]; then
    # Generated from the megadriver's own import list, so it stays right when
    # the driver comes from a different Debian.
    readelf -W --dyn-syms "$R/usr/lib/${MA}/dri/swrast_dri.so" |
        awk '$7 == "UND" && $8 != "" {print $8}' | sed 's/@.*//' | sort -u > "$WORK/undef"
    readelf -W --dyn-syms "$SRC/usr/lib/${MA}/libLLVM-15.so.1" |
        awk '$7 != "UND" && $8 != "" {print $8}' | sed 's/@.*//' | sort -u > "$WORK/llvm"
    comm -12 "$WORK/undef" "$WORK/llvm" > "$WORK/need"
    {
        echo '.text'
        while read -r sym; do
            printf '.globl %s\n.type %s,@function\n%s:\n  %s\n' "$sym" "$sym" "$sym" "$TRAP_INSN"
        done < "$WORK/need"
    } > "$WORK/llvmstub.s"
    # The version, and it is not decoration: the megadriver imports every one
    # of these as `LLVMFoo@LLVM_15`, and against an unversioned definition
    # glibc's ld.so does not warn -- it fails an assertion in dl-lookup.c and
    # kills the process before `main`.
    printf 'LLVM_15 { global: *; };\n' > "$WORK/llvmstub.map"
    "$CC" -shared -nostdlib -o "$R/usr/lib/${MA}/libLLVM-15.so.1" \
        "$WORK/llvmstub.s" -Wl,-soname,libLLVM-15.so.1 \
        -Wl,--version-script="$WORK/llvmstub.map"
    echo "  libLLVM: a stub of $(wc -l < "$WORK/need") symbols, $(du -h "$R/usr/lib/${MA}/libLLVM-15.so.1" | cut -f1)"
else
    for l in libLLVM-15.so.1 libz3.so.4; do
        cp -L "$SRC/usr/lib/${MA}/$l" "$R/usr/lib/${MA}/$l"
    done
    echo "  libLLVM: the real one, $(du -h "$R/usr/lib/${MA}/libLLVM-15.so.1" | cut -f1)"
fi

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
if [ "$MA" != "aarch64-linux-gnu" ]; then
    for f in "$R/gears.sh" "$R/session.sh" "$R/tk-demo.tcl" "$R/root/.twmrc"; do
        [ -f "$f" ] && sed -i "s/aarch64-linux-gnu/$MA/g" "$f"
    done
fi
tar -C "$R" -cf "$OUT" .
echo "  $OUT: $(du -h "$OUT" | cut -f1)"
