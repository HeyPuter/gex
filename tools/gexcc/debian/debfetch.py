#!/usr/bin/env python3
"""Fetch Debian binary packages and their dependency closure, verified.

    debfetch.py --arch ARCH [--suite trixie] [--cache DIR] [--extract ROOT]
                [--no-deps] [--exclude PKG,...] PKG...

The chain of trust: dists/SUITE/InRelease is checked with gpgv against the
host's debian-archive-keyring, binary-ARCH/Packages.xz against InRelease's
SHA256, every .deb against Packages' SHA256. Downloads are cached by file
name under CACHE/pool (default ~/gex-sandbox/scratch/CC/deb), so repeated
runs touch the mirror only for the index (and not even that within a day).

Dependency closure: Pre-Depends and Depends; for "a | b" the first
alternative already in the set wins, else the first that exists. Virtual
packages resolve to their first provider. Prints the chosen packages
(name version arch file), one per line, and with --extract unpacks them
into ROOT (dpkg-deb -x; no maintainer scripts).
"""
import argparse, hashlib, lzma, os, subprocess, sys, time, urllib.request

MIRROR = os.environ.get('GEX_DEBIAN_MIRROR', 'http://deb.debian.org/debian')
KEYRING = '/usr/share/keyrings/debian-archive-keyring.gpg'


def get(url, dest):
    tmp = dest + '.part'
    with urllib.request.urlopen(url, timeout=120) as r, open(tmp, 'wb') as f:
        while True:
            b = r.read(1 << 20)
            if not b:
                break
            f.write(b)
    os.replace(tmp, dest)


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''):
            h.update(b)
    return h.hexdigest()


def fresh(path, max_age=86400):
    return os.path.exists(path) and time.time() - os.path.getmtime(path) < max_age


def release(cache, suite):
    d = f'{cache}/dists/{suite}'
    os.makedirs(d, exist_ok=True)
    inrel = f'{d}/InRelease'
    if not fresh(inrel):
        get(f'{MIRROR}/dists/{suite}/InRelease', inrel)
    subprocess.run(['gpgv', '--keyring', KEYRING, inrel], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sums, on = {}, False
    for line in open(inrel, encoding='utf-8'):
        if line.startswith('SHA256:'):
            on = True
            continue
        if on:
            if not line.startswith(' '):
                on = False
                continue
            h, size, name = line.split()
            sums[name] = h
    return d, sums


def stanzas(text):
    cur = {}
    last = None
    for line in text.splitlines():
        if not line.strip():
            if cur:
                yield cur
            cur, last = {}, None
        elif line[0] in ' \t':
            if last:
                cur[last] += '\n' + line
        else:
            k, _, v = line.partition(':')
            cur[k] = v.strip()
            last = k
    if cur:
        yield cur


def index(cache, suite, kind):
    """kind: 'binary-ARCH' or 'source'."""
    d, sums = release(cache, suite)
    leaf = 'Sources' if kind == 'source' else 'Packages'
    name = f'main/{kind}/{leaf}.xz'
    path = f'{d}/{kind}-{leaf}.xz'
    if not (os.path.exists(path) and sha256(path) == sums[name]):
        get(f'{MIRROR}/dists/{suite}/{name}', path)
        if sha256(path) != sums[name]:
            sys.exit(f'{name}: sha256 does not match InRelease')
    return lzma.open(path, 'rt', encoding='utf-8', errors='replace').read()


def packages(cache, suite, arch):
    pkgs, provides = {}, {}
    for kind in (f'binary-{arch}', 'binary-all'):
        try:
            text = index(cache, suite, kind)
        except KeyError:
            continue  # No-Support-for-Architecture-all: 'all' is in binary-ARCH
        for p in stanzas(text):
            pkgs.setdefault(p['Package'], p)
    for name, p in pkgs.items():
        for item in p.get('Provides', '').split(','):
            v = item.strip().split(' ')[0] if item.strip() else ''
            if v:
                provides.setdefault(v, name)
    return pkgs, provides


def clauses(p):
    out = []
    for field in ('Pre-Depends', 'Depends'):
        for clause in p.get(field, '').split(','):
            alts = [a.strip().split(' ')[0].split(':')[0] for a in clause.split('|') if a.strip()]
            if alts:
                out.append(alts)
    return out


def closure(pkgs, provides, roots, exclude, deps=True):
    chosen, order, todo = set(), [], list(roots)

    def real(n):
        return n if n in pkgs else provides.get(n)

    while todo:
        n = real(todo.pop(0))
        if not n or n in chosen or n in exclude:
            continue
        chosen.add(n)
        order.append(n)
        if not deps:
            continue
        for alts in clauses(pkgs[n]):
            if any(real(a) in chosen or a in exclude for a in alts):
                continue
            pick = next((real(a) for a in alts if real(a)), None)
            if pick:
                todo.append(pick)
    return order


def fetch(cache, p):
    pool = f'{cache}/pool'
    os.makedirs(pool, exist_ok=True)
    path = f'{pool}/{os.path.basename(p["Filename"])}'
    if not (os.path.exists(path) and sha256(path) == p['SHA256']):
        get(f'{MIRROR}/{p["Filename"]}', path)
        if sha256(path) != p['SHA256']:
            sys.exit(f'{path}: sha256 does not match Packages')
    return path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--arch', required=True)
    ap.add_argument('--suite', default='trixie')
    ap.add_argument('--cache', default=os.path.expanduser('~/gex-sandbox/scratch/CC/deb'))
    ap.add_argument('--extract')
    ap.add_argument('--no-deps', action='store_true')
    ap.add_argument('--exclude', default='')
    ap.add_argument('roots', nargs='+')
    a = ap.parse_args()
    pkgs, provides = packages(a.cache, a.suite, a.arch)
    missing = [r for r in a.roots if r not in pkgs and r not in provides]
    if missing:
        sys.exit(f'not in {a.suite}/{a.arch}: {" ".join(missing)}')
    exclude = set(x for x in a.exclude.split(',') if x)
    for n in closure(pkgs, provides, a.roots, exclude, not a.no_deps):
        p = pkgs[n]
        path = fetch(a.cache, p)
        print(n, p['Version'], p['Architecture'], path)
        if a.extract:
            os.makedirs(a.extract, exist_ok=True)
            subprocess.run(['dpkg-deb', '-x', path, a.extract], check=True)


if __name__ == '__main__':
    main()
