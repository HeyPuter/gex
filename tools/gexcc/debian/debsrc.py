#!/usr/bin/env python3
"""Fetch and unpack Debian source packages, verified, with our patches.

    debsrc.py [--suite trixie] [--cache DIR] --into DIR SRCPKG[=VERSION]...

InRelease (gpgv, debian-archive-keyring) -> Sources.xz (SHA256 from
InRelease) -> the .dsc (SHA256 from Sources) -> the tarballs (dpkg-source
checks them against the .dsc). `dpkg-source -x` unpacks and applies
debian/patches; then tools/gexcc/debian/patches/SRCPKG/*.patch are applied
with `patch -p1` in name order. A VERSION pin that no longer matches the
index fails (a Debian point release changes the pin explicitly).

Prints `SRCPKG VERSION DIR` per package. An existing DIR/SRCPKG-VERSION is
left alone (delete it to re-unpack).
"""
import argparse, glob, os, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import debfetch  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def sources(cache, suite):
    out = {}
    for s in debfetch.stanzas(debfetch.index(cache, suite, 'source')):
        # Several stanzas can share a name (rare); the last (newest) wins.
        out[s['Package']] = s
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--suite', default='trixie')
    ap.add_argument('--cache', default=os.path.expanduser('~/gex-sandbox/scratch/CC/deb'))
    ap.add_argument('--into', required=True)
    ap.add_argument('pkgs', nargs='+')
    a = ap.parse_args()
    idx = sources(a.cache, a.suite)
    os.makedirs(a.into, exist_ok=True)
    for spec in a.pkgs:
        name, _, pin = spec.partition('=')
        s = idx.get(name)
        if not s:
            sys.exit(f'{name}: no such source package in {a.suite}')
        ver = s['Version']
        if pin and pin != ver:
            sys.exit(f'{name}: pinned {pin}, {a.suite} has {ver}')
        upver = ver.split(':', 1)[-1]
        d = os.path.join(a.into, f'{name}-{upver}')
        if not os.path.isdir(d):
            sums = {}
            for line in s.get('Checksums-Sha256', '').splitlines():
                parts = line.split()
                if len(parts) == 3:
                    sums[parts[2]] = parts[0]
            pool = os.path.join(a.cache, 'src', name)
            os.makedirs(pool, exist_ok=True)
            dsc = None
            for fn, h in sums.items():
                path = os.path.join(pool, fn)
                if not (os.path.exists(path) and debfetch.sha256(path) == h):
                    debfetch.get(f'{debfetch.MIRROR}/{s["Directory"]}/{fn}', path)
                    if debfetch.sha256(path) != h:
                        sys.exit(f'{fn}: sha256 does not match Sources')
                if fn.endswith('.dsc'):
                    dsc = path
            # The .dsc's own signature is not checked (--no-check): its
            # hash came from the signed index, which is the stronger chain.
            subprocess.run(['dpkg-source', '--no-check', '--require-strong-checksums', '-x', dsc, d],
                           check=True, stdout=subprocess.DEVNULL)
            for p in sorted(glob.glob(os.path.join(HERE, 'patches', name, '*.patch'))):
                subprocess.run(['patch', '-p1', '-s', '-i', p], cwd=d, check=True)
        print(name, ver, d)


if __name__ == '__main__':
    main()
