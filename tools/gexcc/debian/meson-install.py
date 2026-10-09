#!/usr/bin/env python3
"""`meson install` for gexcc builds: meson-install.py -C build [args].

meson rewrites the rpath of every ELF it installs, and its ELF reader fails
on a gexcc program's container (a 32-bit ELF with no section headers: a
wasm module wrapped for gex). Such a file has no rpath to fix; skip it.
"""
import sys
from mesonbuild.scripts import depfixer

_fix = depfixer.fix_rpath


def fix_rpath(fname, *args, **kwargs):
    with open(fname, 'rb') as f:
        h = f.read(52)
    if h[:4] == b'\x7fELF' and h[4] == 1 and int.from_bytes(h[48:50], 'little') == 0:
        return
    return _fix(fname, *args, **kwargs)


depfixer.fix_rpath = fix_rpath
from mesonbuild import mesonmain  # noqa: E402
sys.argv = ['meson', 'install'] + sys.argv[1:]
sys.exit(mesonmain.main())
