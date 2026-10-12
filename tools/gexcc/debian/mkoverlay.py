#!/usr/bin/env python3
"""A launcher package for the demo desktop (docs/gexcc.md §9): what TREE has
that BASE does not, as a tar.

Both trees are mkrootfs.py images, TREE built with BASE's packages and more.
The tar holds every file, symlink and directory of TREE that BASE lacks or
has different, except the dpkg status (one file every image has); instead
the stanzas of the packages BASE lacks go in var/lib/gex-overlay/NAME.status,
and the page appends them to the running system's status
(web/src/overlay-worker.js).

  mkoverlay.py --base BASE --tree TREE --name NAME --tar OUT.tar
"""
import argparse
import hashlib
import io
import os
import tarfile

SKIP = {'var/lib/dpkg/status', 'var/lib/dpkg/status-old', 'var/lib/dpkg/available'}


def stanzas(path):
    text = open(path, encoding='utf-8', errors='replace').read() if os.path.exists(path) else ''
    out = {}
    for st in text.split('\n\n'):
        st = st.strip('\n')
        if not st:
            continue
        f = dict(l.split(': ', 1) for l in st.split('\n') if ': ' in l and not l.startswith(' '))
        out[(f.get('Package'), f.get('Architecture'))] = st
    return out


def digest(p):
    h = hashlib.sha256()
    with open(p, 'rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''):
            h.update(b)
    return h.digest()


def same(a, b):
    if os.path.islink(a) or os.path.islink(b):
        return os.path.islink(a) and os.path.islink(b) and os.readlink(a) == os.readlink(b)
    if os.path.isdir(a) or os.path.isdir(b):
        return os.path.isdir(a) and os.path.isdir(b)
    if not os.path.lexists(b):
        return False
    sa, sb = os.lstat(a), os.lstat(b)
    return sa.st_size == sb.st_size and (sa.st_mode & 0o111) == (sb.st_mode & 0o111) and digest(a) == digest(b)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--base', required=True)
    ap.add_argument('--tree', required=True)
    ap.add_argument('--name', required=True)
    ap.add_argument('--tar', required=True)
    a = ap.parse_args()
    rels = []
    for d, dirs, files in os.walk(a.tree):
        dirs.sort()
        for n in sorted(dirs) + sorted(files):
            p = os.path.join(d, n)
            rel = os.path.relpath(p, a.tree)
            if rel in SKIP:
                continue
            if os.path.isdir(p) and not os.path.islink(p):
                if not os.path.isdir(os.path.join(a.base, rel)):
                    rels.append(rel)
                continue
            if not same(p, os.path.join(a.base, rel)):
                rels.append(rel)
    have = stanzas(os.path.join(a.base, 'var/lib/dpkg/status'))
    new = [st for k, st in stanzas(os.path.join(a.tree, 'var/lib/dpkg/status')).items() if k not in have]
    with tarfile.open(a.tar, 'w', format=tarfile.GNU_FORMAT) as tf:
        for rel in rels:
            ti = tf.gettarinfo(os.path.join(a.tree, rel), arcname=rel)
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = 'root'
            if ti.isreg():
                with open(os.path.join(a.tree, rel), 'rb') as f:
                    tf.addfile(ti, f)
            else:
                tf.addfile(ti)
        body = ('\n\n'.join(new) + '\n\n').encode() if new else b''
        ti = tarfile.TarInfo(f'var/lib/gex-overlay/{a.name}.status')
        ti.size = len(body)
        ti.mode = 0o644
        tf.addfile(ti, io.BytesIO(body))
    names = sorted(st.split('\n', 1)[0].split(': ', 1)[1] for st in new)
    print(f'{a.name}: {len(rels)} paths, {len(new)} packages: {" ".join(names)}')


if __name__ == '__main__':
    main()
