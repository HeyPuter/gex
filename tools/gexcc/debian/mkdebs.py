#!/usr/bin/env python3
"""Split a staging tree into Debian binary packages and build the .debs.

    mkdebs.py --src SRCDIR --stage STAGE --out POOL SRCPKG VERSION

The split comes from binpkgs.txt (`SRCPKG BINPKG GLOB...`, globs relative to
the staging root, in order; a file goes to the first package whose glob
matches it; a source package with no lines there becomes one binary package
of its own name). Control fields come from the source's debian/control
stanza for that binary package: Section, Priority, Essential, Multi-Arch,
Description; the version is Debian's plus `+gex1`, the architecture
`musl-linux-wasm32`. Tools are `Multi-Arch: foreign` so that they satisfy
arm64 packages' dependencies by name (docs/gexcc.md §6.1a). Programs link
musl statically, so there are no library dependencies to record.

Prints `BINPKG VERSION PATH` per .deb.
"""
import argparse, fnmatch, hashlib, os, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ARCH = 'musl-linux-wasm32'
# development files: the rootfs runs programs, it does not build against
# these (static archives and headers stay in the gexcc sysroot)
DROP = ['usr/include/*', 'usr/lib/*.a', 'usr/lib/pkgconfig/*', 'usr/share/pkgconfig/*',
        'usr/share/aclocal/*', 'usr/share/info/*', 'usr/lib/*.la', 'usr/share/man/man3/*']
SUFFIX = '+gex1'
# binpkgs.txt lines that are not splits
KINDS = ('depends', 'predepends', 'arch')


def control_stanzas(path):
    out, cur, last = [], {}, None
    for line in open(path, encoding='utf-8'):
        line = line.rstrip('\n')
        if line.startswith('#'):
            continue
        if not line.strip():
            if cur:
                out.append(cur)
            cur, last = {}, None
        elif line[0] in ' \t' and last:
            cur[last] += '\n' + line
        else:
            k, _, v = line.partition(':')
            cur[k.strip()] = v.strip()
            last = k.strip()
    if cur:
        out.append(cur)
    return out


def split_rules(srcpkg):
    rules = []
    for line in open(os.path.join(HERE, 'binpkgs.txt')):
        f = line.split()
        if f and not f[0].startswith('#') and f[0] == srcpkg and f[0] not in KINDS:
            rules.append((f[1], f[2:]))
    return rules or [(srcpkg, ['*'])]


def field(kind, binpkg):
    """`KIND BINPKG VALUE...` lines of binpkgs.txt: the rest of each line,
    joined with ", " (a Depends field: `depends dpkg tar (>= 1.28-1)`)."""
    out = []
    for line in open(os.path.join(HERE, 'binpkgs.txt')):
        f = line.split(None, 2)
        if len(f) > 2 and f[0] == kind and f[1] == binpkg:
            out.append(f[2].strip())
    return ', '.join(out)


def depends(binpkg):
    return field('depends', binpkg)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--src', required=True)
    ap.add_argument('--stage', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('srcpkg')
    ap.add_argument('version')
    a = ap.parse_args()
    stanzas = control_stanzas(os.path.join(a.src, 'debian', 'control'))
    source = stanzas[0]
    bins = {s['Package']: s for s in stanzas[1:] if 'Package' in s}
    rules = split_rules(a.srcpkg)
    owned = {b: [] for b, _ in rules}
    for root, dirs, files in os.walk(a.stage):
        # (an empty directory is part of a package too: python's lib-dynload)
        empty = [] if (files or dirs or root == a.stage) else ['']
        for n in files + [d for d in dirs if os.path.islink(os.path.join(root, d))] + empty:
            rel = os.path.relpath(os.path.join(root, n) if n else root, a.stage)
            if any(fnmatch.fnmatch(rel, g) for g in DROP):
                continue
            for b, globs in rules:
                if any(fnmatch.fnmatch(rel, g) for g in globs):
                    owned[b].append(rel)
                    break
    version = a.version + SUFFIX
    os.makedirs(a.out, exist_ok=True)
    for b, rels in owned.items():
        if b == '-':
            continue
        if not rels:
            print(f'{b}: no files', file=sys.stderr)
            continue
        st = bins.get(b, {})
        # dpkg and apt are labelled with the native architecture, arm64
        # (docs/gexcc.md §6.1a): `arch dpkg arm64`
        arch = field('arch', b) or ARCH
        with tempfile.TemporaryDirectory() as t:
            for rel in rels:
                s, d = os.path.join(a.stage, rel), os.path.join(t, rel)
                os.makedirs(os.path.dirname(d), exist_ok=True)
                if os.path.isdir(s) and not os.path.islink(s):
                    os.makedirs(d, exist_ok=True)
                elif os.path.islink(s):
                    os.symlink(os.readlink(s), d)
                else:
                    shutil.copy2(s, d)
            size = sum(os.path.getsize(os.path.join(t, r)) for r in rels
                       if os.path.isfile(os.path.join(t, r)) and not os.path.islink(os.path.join(t, r))) // 1024
            os.makedirs(os.path.join(t, 'DEBIAN'))
            md5 = []
            for rel in sorted(rels):
                p = os.path.join(t, rel)
                if os.path.isfile(p) and not os.path.islink(p):
                    md5.append(f'{hashlib.md5(open(p, "rb").read()).hexdigest()}  {rel}')
            open(os.path.join(t, 'DEBIAN', 'md5sums'), 'w').write('\n'.join(md5) + '\n')
            ctl = [f'Package: {b}', f'Source: {a.srcpkg}' + (f' ({a.version})' if a.version else ''),
                   f'Version: {version}', f'Architecture: {arch}',
                   'Maintainer: gex native builds <gex@localhost>',
                   f'Original-Maintainer: {source.get("Maintainer", "unknown")}',
                   f'Installed-Size: {size}']
            for k in ('Essential', 'Section', 'Priority'):
                if k in st or k in source:
                    ctl.append(f'{k}: {st.get(k, source.get(k))}')
            ma = st.get('Multi-Arch', 'foreign')
            ctl.append(f'Multi-Arch: {"foreign" if ma in ("foreign", "allowed", "no") else ma}')
            # relations from debian/control, without the substitution
            # variables dh would have filled (library dependencies, which a
            # static program does not have)
            for k in ('Provides', 'Conflicts', 'Breaks', 'Replaces'):
                v = ', '.join(x.strip() for x in st.get(k, '').replace('\n', ' ').split(',')
                              if x.strip() and '${' not in x)
                if v:
                    ctl.append(f'{k}: {v}')
            if field('predepends', b):
                ctl.append(f"Pre-Depends: {field('predepends', b)}")
            if depends(b):
                ctl.append(f'Depends: {depends(b)}')
            if 'Homepage' in source:
                ctl.append(f'Homepage: {source["Homepage"]}')
            desc = st.get('Description', f'{b} (native wasm32 build)')
            ctl.append(f'Description: {desc}')
            open(os.path.join(t, 'DEBIAN', 'control'), 'w').write('\n'.join(ctl) + '\n')
            deb = os.path.join(a.out, f'{b}_{version.split(":", 1)[-1]}_{arch}.deb')
            subprocess.run(['dpkg-deb', '--root-owner-group', '-Zxz', '--build', t, deb],
                           check=True, stdout=subprocess.DEVNULL)
            print(b, version, deb)


if __name__ == '__main__':
    main()
