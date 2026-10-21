#!/bin/sh
# Rebuilds the PE fixtures in bin/ from src/. The binaries are committed, so
# `cargo test` needs no MinGW; run this only after changing a source. Needs
# i686-w64-mingw32-gcc, x86_64-w64-mingw32-gcc and (for the MSVC-style
# image) clang-cl, lld-link and llvm-dlltool.
#
#   build.sh [OUT]    (default: bin/ beside this script)
#
# The committed ones were built with MinGW-w64 GCC 15.2. Binutils 2.40's
# dlltool leaves uses.exe without its delay import, which tests/pe.rs checks.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src=$here/src
out=${1:-$here/bin}
rm -rf "$out"
mkdir -p "$out/x86" "$out/x64"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for arch in x86 x64; do
    if [ $arch = x86 ]; then
        cc=i686-w64-mingw32-gcc; tool=i686-w64-mingw32-dlltool; entry=_entry; dllentry=_DllMain@12
        res=i686-w64-mingw32-windres
    else
        cc=x86_64-w64-mingw32-gcc; tool=x86_64-w64-mingw32-dlltool; entry=entry; dllentry=DllMain
        res=x86_64-w64-mingw32-windres
    fi
    o=$out/$arch
    flags="-O1 -s -ffile-prefix-map=$here=. -fno-stack-protector -fno-asynchronous-unwind-tables"
    # The DLLs: no CRT, their own entry point, exports from .def files.
    $cc $flags -shared -nostdlib -Wl,--entry,$dllentry "$src/libb.c" "$src/libb.def" -lkernel32 -o "$o/libb.dll"
    $tool -d "$src/libb.def" -D libb.dll -l "$tmp/libb-$arch.a"
    $cc $flags -shared -nostdlib -Wl,--entry,$dllentry "$src/liba.c" "$src/liba.def" "$tmp/libb-$arch.a" -lkernel32 -o "$o/liba.dll"
    $tool -d "$src/liba.def" -D liba.dll -l "$tmp/liba-$arch.a"
    $cc $flags -shared -nostdlib -Wl,--entry,$dllentry "$src/delayed.c" "$src/delayed.def" -lkernel32 -o "$o/delayed.dll"
    $tool -d "$src/delayed.def" -D delayed.dll -y "$tmp/delayed-$arch.a"
    # The application: the MinGW CRT, TLS callbacks, delay imports, resources.
    $res "$src/uses.rc" -O coff -o "$tmp/uses-res-$arch.o"
    $cc $flags "$src/uses.c" "$tmp/uses-res-$arch.o" "$tmp/liba-$arch.a" "$tmp/delayed-$arch.a" -o "$o/uses.exe"
    # The program fixtures, each built for the width its name implies.
    for f in "$src"/*.c; do
        n=$(basename "$f" .c)
        case $n in liba|libb|delayed|uses|tiny|msvc) continue ;; esac
        case $n in *32) [ $arch = x86 ] || continue ;; *) [ $arch = x64 ] || continue ;; esac
        # threads32 prints through the UCRT (api-ms-win-crt-stdio), the rest
        # import msvcrt.
        crt="-lmsvcrt"; [ $n = threads32 ] && crt="-D__MSVCRT_VERSION__=0xE00 -D_UCRT -lucrt"
        $cc $flags -nostdlib -Wl,--entry,$entry "$f" -lkernel32 -luser32 -lgdi32 $crt \
            -ld3d11 -ldxgi -ldsound -ldinput8 -ldxguid -lopengl32 -lwinmm -lole32 -o "$o/$n.exe"
    done
done

# PE32+ at a high image base, relocatable and not.
x=x86_64-w64-mingw32-gcc
xf="-O1 -s -fno-stack-protector -nostdlib -Wl,--entry,entry"
$x $xf -Wl,--image-base,0x140000000 -Wl,--dynamicbase "$src/tiny.c" -lkernel32 -o "$out/x64/highbase.exe"
$x $xf -Wl,--image-base,0x140000000 -Wl,--disable-dynamicbase -Wl,--disable-reloc-section "$src/tiny.c" -lkernel32 -o "$out/x64/highbase-fixed.exe"

# An MSVC-style image: clang-cl and lld-link, no MSVC libraries (an import
# library made from a .def by llvm-dlltool, and our own entry point).
if command -v clang-cl >/dev/null && command -v lld-link >/dev/null && command -v llvm-dlltool >/dev/null; then
    printf 'LIBRARY kernel32.dll\nEXPORTS\n  GetStdHandle\n  WriteFile\n' > "$tmp/k32.def"
    llvm-dlltool -m i386:x86-64 -d "$tmp/k32.def" -l "$tmp/k32-x64.lib"
    clang-cl --target=x86_64-pc-windows-msvc /O1 /GS- /c "$src/msvc.c" /Fo"$tmp/tiny64.obj"
    lld-link /nodefaultlib /entry:entry /subsystem:console /dynamicbase "$tmp/tiny64.obj" "$tmp/k32-x64.lib" /out:"$out/x64/msvc.exe"
    printf 'LIBRARY kernel32.dll\nEXPORTS\n  GetStdHandle@4\n  WriteFile@20\n' > "$tmp/k32-32.def"
    llvm-dlltool -k -m i386 -d "$tmp/k32-32.def" -l "$tmp/k32-x86.lib"
    clang-cl --target=i686-pc-windows-msvc /O1 /GS- /c "$src/msvc.c" /Fo"$tmp/tiny32.obj"
    lld-link /nodefaultlib /entry:entry /subsystem:console /dynamicbase /safeseh:no "$tmp/tiny32.obj" "$tmp/k32-x86.lib" /out:"$out/x86/msvc.exe"
fi
ls -l "$out"/*
