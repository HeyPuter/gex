# rootfs manifests

The deployed graphics rootfs (`web/public/rootfs/graphics.tar`, gitignored) is a
curated NEEDED-closure subset of a much larger apt-grafted tree. These manifests
record which files each heavy app contributes on top of the base tar, so the tar
can be reconstructed without re-deriving the closure by hand.

## dolphin-delta.txt

The 599 files Dolphin (KDE/Qt5 file manager, 22.12) adds beyond the base
graphics tar. Almost all of Dolphin's linked libraries are already present
(kcalc/compton-conf pulled the Qt5+KF5 closure); this delta is the KIO worker
binaries, Baloo, Dolphin's own parts/kcms/config, and the ~19 remaining NEEDED
libs. Docs, man pages, non-English locale, and every Mesa DRI driver except
`swrast_dri.so` are deliberately excluded (they added ~1.1 GiB of dead weight).

Derivation (from a graft tree at `/tmp/gex-graphics` that has the full closure):

1. `have.txt` = `tar tf graphics.tar` (files already in the base).
2. NEEDED-closure walk of `/usr/bin/dolphin` against the tree's lib dirs, minus
   `have.txt`, gives the mandatory missing libs.
3. Union with Dolphin's runtime dlopen dirs (libexec/kf5, kf5 plugins,
   kservices5, share/dolphin, config.kcfg), then a second NEEDED-walk over every
   added `.so` to pull transitive deps (converged at +2 libs).
4. Strip docs/man/locale/lintian and all `dri/*` except `swrast_dri.so`.

Rebuild: `tar --append -f graphics.tar -C <graft-tree> -T dolphin-delta.txt`
(the base tar is uncompressed, so append works). Validate headless at node with
`GEX_WL_STUB=1 GEX_WL_DUMP=out.ppm` before deploying — a real render is ~1000+
distinct colors in the 640x480 dump.
