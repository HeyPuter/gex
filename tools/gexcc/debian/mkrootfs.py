#!/usr/bin/env python3
"""Assemble the native Debian rootfs (docs/gexcc.md §6): Debian's arm64
system pieces (dpkg, apt, perl-base, libc6, ...), Debian's architecture-
independent data, and our musl-linux-wasm32 packages in place of Debian's
tools, with a dpkg database that says so and apt set up for both.

    mkrootfs.py --pool POOL --out DIR [--tar FILE] [--roots PKG,...]

No root and no maintainer scripts: every .deb is unpacked (dpkg-deb
--fsys-tarfile) into DIR with /bin, /sbin and /lib as /usr's (merged /usr);
/var/lib/dpkg/status, info/*.list, *.md5sums and *.conffiles are written
from the packages; what the scripts would have done for this closure
(accounts, /etc/shells, alternatives, the CA bundle, /root's dot files) is
done here. Debian's packages are verified (debfetch.py: InRelease ->
Packages -> .deb) and cached.

Our packages (POOL, from mkdebs.py) stand in for Debian's by name: a
dependency an arm64 package has on `coreutils` or `awk` is met by ours
(Multi-Arch: foreign), and Debian's copy is not unpacked. apt: Debian's
repository for arm64 only, ours (Origin gex) as a file: repository at
/var/lib/gex-apt for musl-linux-wasm32, pinned so that apt never replaces
one of ours with Debian's (docs/gexcc.md §6.1a).
"""
import argparse, glob, gzip, hashlib, io, lzma, os, shutil, subprocess, sys, tarfile, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import debfetch  # noqa: E402

a_minimal = True

OURS_ARCH = 'musl-linux-wasm32'
NATIVE = 'arm64'
SUITE = 'trixie'
# The arm64 roots: what apt and dpkg need to run, and what the image needs
# to be a Debian system (the rest of Essential comes from our pool).
ROOTS = ['apt', 'dpkg', 'base-files', 'base-passwd', 'debian-archive-keyring', 'ncurses-base',
         'libc-bin', 'perl-base', 'debconf', 'ca-certificates', 'usr-is-merged', 'sensible-utils',
         'netbase', 'init-system-helpers', 'login.defs']
MIRROR_URI = 'http://deb.debian.org/debian'
SECURITY_URI = 'http://security.debian.org/debian-security'


def control(deb):
    out = subprocess.run(['dpkg-deb', '-f', deb], check=True, capture_output=True, text=True).stdout
    return next(debfetch.stanzas(out))


def control_member(deb, name):
    """A file of the control archive (md5sums, conffiles), or None."""
    with tempfile.TemporaryDirectory() as t:
        subprocess.run(['dpkg-deb', '-e', deb, t], check=True)
        p = os.path.join(t, name)
        return open(p, encoding='utf-8', errors='replace').read() if os.path.exists(p) else None


# As Debian's minimized images (and dpkg's path-exclude, which the image
# carries so apt installs behave the same): no documentation or
# translations; copyright files stay.
EXCLUDE = ['/usr/share/doc/*', '/usr/share/man/*', '/usr/share/info/*', '/usr/share/locale/*',
           '/usr/share/lintian/*', '/usr/share/linda/*', '/usr/share/groff/*',
           # glibc's iconv modules (20 MB): the Unicode and Western ones stay
           # (UTF-8 and ISO-8859-1 are built into libc)
           '/usr/lib/*/gconv/*.so']
INCLUDE = ['/usr/share/doc/*/copyright', '/usr/lib/*/gconv/UTF-*.so', '/usr/lib/*/gconv/UNICODE.so',
           '/usr/lib/*/gconv/ISO8859-15.so', '/usr/lib/*/gconv/CP1252.so', '/usr/lib/*/gconv/libGB.so',
           '/usr/lib/*/gconv/libJIS.so', '/usr/lib/*/gconv/EUC-JP.so', '/usr/lib/*/gconv/SJIS.so']


def excluded(path):
    import fnmatch
    return any(fnmatch.fnmatch(path, g) for g in EXCLUDE) and not any(fnmatch.fnmatch(path, g) for g in INCLUDE)


def unpack(deb, root):
    """Unpack the data archive; returns the paths it holds (dpkg's .list)."""
    data = subprocess.run(['dpkg-deb', '--fsys-tarfile', deb], check=True, capture_output=True).stdout
    paths = []
    with tarfile.open(fileobj=io.BytesIO(data)) as tf:
        for m in tf.getmembers():
            rel = os.path.normpath(m.name)
            if rel == '.':
                paths.append('/.')
                continue
            if a_minimal and excluded('/' + rel) and not m.isdir():
                continue
            paths.append('/' + rel)
            dest = os.path.join(root, real_rel(root, rel))
            if m.isdir():
                if not os.path.isdir(dest):
                    os.makedirs(dest, exist_ok=True)
                os.chmod(dest, m.mode | 0o700)
                continue
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            if m.issym() and os.path.islink(dest) and os.readlink(dest) == m.linkname:
                continue  # base-files' /bin -> usr/bin, made already
            if os.path.islink(dest) or (os.path.lexists(dest) and not os.path.isdir(dest)):
                os.unlink(dest)
            if m.issym():
                os.symlink(m.linkname, dest)
            elif m.islnk():
                os.link(os.path.join(root, real_rel(root, os.path.normpath(m.linkname))), dest)
            elif m.isfile():
                with tf.extractfile(m) as src, open(dest, 'wb') as f:
                    shutil.copyfileobj(src, f)
                os.chmod(dest, m.mode & 0o7777 | 0o600)
    return paths


def real_rel(root, rel):
    """`rel` with merged-/usr aliases followed (bin -> usr/bin ...)."""
    top, _, rest = rel.partition('/')
    if rest and top in ('bin', 'sbin', 'lib') and os.path.islink(os.path.join(root, top)):
        return os.path.join('usr', top, rest)
    return rel


def md5(path):
    return hashlib.md5(open(path, 'rb').read()).hexdigest()


def status_stanza(ctl, conffiles_md5):
    keys = list(ctl)
    out = [f'Package: {ctl["Package"]}', 'Status: install ok installed']
    for k in keys:
        if k in ('Package', 'Filename', 'Size', 'MD5sum', 'SHA256', 'SHA1', 'Description-md5'):
            continue
        out.append(f'{k}: {ctl[k]}')
    if conffiles_md5:
        out.append('Conffiles:')
        out += [f' {p} {h}' for p, h in conffiles_md5]
    return '\n'.join(out) + '\n'


def info_name(ctl):
    """dpkg's info file stem: `pkg:arch` for a Multi-Arch: same package."""
    if ctl.get('Multi-Arch') == 'same':
        return f'{ctl["Package"]}:{ctl["Architecture"]}'
    return ctl['Package']


def write_repo(root, debs):
    """Our packages as an apt repository at /var/lib/gex-apt (Packages,
    Release; the .debs themselves are installed already and not copied
    unless --repo-debs)."""
    repo = os.path.join(root, 'var/lib/gex-apt')
    bindir = os.path.join(repo, f'dists/{SUITE}/main/binary-{OURS_ARCH}')
    os.makedirs(bindir, exist_ok=True)
    stanzas = []
    for deb, ctl in debs:
        name = ctl['Package']
        fn = f'pool/main/{name[0]}/{name}/{os.path.basename(deb)}'
        data = open(deb, 'rb').read()
        s = [f'{k}: {v}' for k, v in ctl.items()]
        s += [f'Filename: {fn}', f'Size: {len(data)}', f'MD5sum: {hashlib.md5(data).hexdigest()}',
              f'SHA256: {hashlib.sha256(data).hexdigest()}']
        stanzas.append('\n'.join(s) + '\n')
    text = '\n'.join(stanzas).encode()
    open(os.path.join(bindir, 'Packages'), 'wb').write(text)
    open(os.path.join(bindir, 'Packages.xz'), 'wb').write(lzma.compress(text))
    import email.utils
    rel = [f'Origin: gex', 'Label: gex native', f'Suite: {SUITE}', f'Codename: {SUITE}',
           f'Date: {email.utils.formatdate(usegmt=True)}',
           f'Architectures: {OURS_ARCH}', 'Components: main',
           'Description: Debian tools built from source with gexcc as native wasm32', 'SHA256:']
    for leaf in ('Packages', 'Packages.xz'):
        p = os.path.join(bindir, leaf)
        rel.append(f' {hashlib.sha256(open(p, "rb").read()).hexdigest()} {os.path.getsize(p)} '
                   f'main/binary-{OURS_ARCH}/{leaf}')
    open(os.path.join(repo, f'dists/{SUITE}/Release'), 'w').write('\n'.join(rel) + '\n')
    return repo


def configure(root, ours, native_pkgs):
    """What maintainer scripts and debootstrap would have done."""
    def w(rel, text, mode=0o644):
        p = os.path.join(root, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, 'w').write(text)
        os.chmod(p, mode)

    # base-passwd
    pw = os.path.join(root, 'usr/share/base-passwd')
    shutil.copy(os.path.join(pw, 'passwd.master'), os.path.join(root, 'etc/passwd'))
    shutil.copy(os.path.join(pw, 'group.master'), os.path.join(root, 'etc/group'))
    users = [l.split(':')[0] for l in open(os.path.join(root, 'etc/passwd'))]
    groups = [l.split(':')[0] for l in open(os.path.join(root, 'etc/group'))]
    w('etc/shadow', ''.join(f'{u}:*:20000:0:99999:7:::\n' for u in users), 0o640)
    w('etc/gshadow', ''.join(f'{g}:*::\n' for g in groups), 0o640)
    # /etc/shells: debianutils' list plus every shells.d file (add-shell)
    shells = open(os.path.join(root, 'usr/share/debianutils/shells')).read() \
        if os.path.exists(os.path.join(root, 'usr/share/debianutils/shells')) else '# /etc/shells: valid login shells\n/bin/sh\n'
    for f in sorted(glob.glob(os.path.join(root, 'usr/share/debianutils/shells.d/*'))):
        shells += open(f).read()
    seen = []
    for l in shells.splitlines():
        if l not in seen:
            seen.append(l)
    w('etc/shells', '\n'.join(seen) + '\n')
    # base-files' postinst: /root's dot files, /etc/os-release is shipped
    for n in ('profile', 'bashrc'):
        src = os.path.join(root, f'usr/share/base-files/dot.{n}')
        if os.path.exists(src):
            os.makedirs(os.path.join(root, 'root'), exist_ok=True)
            shutil.copy(src, os.path.join(root, f'root/.{n}'))
    os.chmod(os.path.join(root, 'root'), 0o700)
    # and its install_from_default: /etc/profile (PS1, PATH), /etc/motd
    for n in ('profile', 'motd'):
        src = os.path.join(root, f'usr/share/base-files/{n}')
        if os.path.exists(src) and not os.path.lexists(os.path.join(root, f'etc/{n}')):
            shutil.copy(src, os.path.join(root, f'etc/{n}'))
    for d in ('tmp', 'var/tmp'):
        os.makedirs(os.path.join(root, d), exist_ok=True)
        os.chmod(os.path.join(root, d), 0o1777)
    # ca-certificates: the configuration and the bundle update-ca-certificates writes
    crts = sorted(glob.glob(os.path.join(root, 'usr/share/ca-certificates/mozilla/*.crt')))
    w('etc/ca-certificates.conf', ''.join(f'mozilla/{os.path.basename(c)}\n' for c in crts))
    os.makedirs(os.path.join(root, 'etc/ssl/certs'), exist_ok=True)
    w('etc/ssl/certs/ca-certificates.crt', ''.join(open(c).read().rstrip('\n') + '\n' for c in crts))
    # alternatives the postinsts register (the highest priority of what is here)
    alts = {
        'awk': [('/usr/bin/mawk', 5), ('/usr/bin/gawk', 10)],
        'editor': [('/usr/bin/nano', 40), ('/usr/bin/vim.basic', 30)],
        'vi': [('/usr/bin/vim.basic', 30)], 'vim': [('/usr/bin/vim.basic', 30)],
        'view': [('/usr/bin/vim.basic', 30)], 'ex': [('/usr/bin/vim.basic', 30)],
        'pager': [('/usr/bin/less', 77), ('/usr/bin/more', 50)],
    }
    links = {'awk': '/usr/bin/awk', 'editor': '/usr/bin/editor', 'vi': '/usr/bin/vi', 'vim': '/usr/bin/vim',
             'view': '/usr/bin/view', 'ex': '/usr/bin/ex', 'pager': '/usr/bin/pager'}
    os.makedirs(os.path.join(root, 'etc/alternatives'), exist_ok=True)
    os.makedirs(os.path.join(root, 'var/lib/dpkg/alternatives'), exist_ok=True)
    for name, choices in alts.items():
        have = [(p, pr) for p, pr in choices if os.path.lexists(os.path.join(root, p.lstrip('/')))]
        if not have:
            continue
        best = max(have, key=lambda c: c[1])[0]
        alt = os.path.join(root, 'etc/alternatives', name)
        link = os.path.join(root, links[name].lstrip('/'))
        for p in (alt, link):
            if os.path.lexists(p):
                os.unlink(p)
        os.symlink(best, alt)
        os.symlink(f'/etc/alternatives/{name}', link)
        state = ['auto', links[name], '']
        for p, pr in have:
            state += [p, str(pr)]
        w(f'var/lib/dpkg/alternatives/{name}', '\n'.join(state) + '\n\n')
    # the system's identity and name service
    w('etc/hostname', 'gex\n')
    w('etc/hosts', '127.0.0.1\tlocalhost\n127.0.1.1\tgex\n::1\t\tlocalhost ip6-localhost ip6-loopback\n')
    w('etc/resolv.conf', 'nameserver 1.1.1.1\n')
    w('etc/fstab', '# gex: no block devices\n')
    if not os.path.exists(os.path.join(root, 'etc/nsswitch.conf')):
        w('etc/nsswitch.conf', 'passwd: files\ngroup: files\nshadow: files\nhosts: files dns\n'
          'networks: files\nprotocols: files\nservices: files\nethers: files\nrpc: files\n')
    # apt: Debian for arm64, ours for musl-linux-wasm32, the pins
    w('etc/apt/sources.list.d/debian.sources',
      f'Types: deb\nURIs: {MIRROR_URI}\nSuites: {SUITE} {SUITE}-updates\nComponents: main\n'
      f'Architectures: {NATIVE}\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\n'
      f'Types: deb\nURIs: {SECURITY_URI}\nSuites: {SUITE}-security\nComponents: main\n'
      f'Architectures: {NATIVE}\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n')
    w('etc/apt/sources.list.d/gex.sources',
      f'Types: deb\nURIs: file:/var/lib/gex-apt\nSuites: {SUITE}\nComponents: main\n'
      f'Architectures: {OURS_ARCH}\nTrusted: yes\n')
    names = sorted({c['Package'] for _, c in ours})
    w('etc/apt/preferences.d/gex-native',
      '# Our native tools (musl-linux-wasm32) are never replaced by Debian\'s\n'
      '# arm64 builds; libraries are not pinned (docs/gexcc.md §6.1a).\n'
      f'Package: {" ".join(n + ":" + NATIVE for n in names)}\nPin: release *\nPin-Priority: -1\n\n'
      'Package: *\nPin: release o=gex\nPin-Priority: 1001\n')
    w('etc/apt/apt.conf.d/99gex',
      '// gex: one user, no sandbox user to drop to\n'
      'APT::Sandbox::User "root";\n'
      '// no translations: apt-get update fetches the Packages indexes only\n'
      'Acquire::Languages "none";\n'
      '// the trixie arm64 index and ours, in one mapping (fork copies it: no\n'
      '// copy-on-write in the guest, so no bigger than needed)\n'
      'APT::Cache-Start "100663296";\n'
      'Dpkg::Use-Pty "false";\n'
      'APT::Install-Recommends "false";\n'
      '// said here so apt does not fork dpkg to ask (two dpkg runs, 3 s of\n'
      '// every apt-cache in the page)\n'
      f'APT::Architecture "{NATIVE}";\n'
      f'APT::Architectures {{ "{NATIVE}"; "{OURS_ARCH}"; }};\n')
    for d in ('var/lib/apt/lists/partial', 'var/cache/apt/archives/partial', 'var/lib/dpkg/updates',
              'var/lib/dpkg/triggers', 'var/log/apt', 'etc/apt/trusted.gpg.d', 'etc/apt/keyrings',
              'etc/dpkg/dpkg.cfg.d', 'var/cache/debconf', 'var/lib/apt/periodic', 'proc', 'dev', 'run', 'sys',
              'home', 'mnt', 'srv', 'opt', 'media', 'usr/local/bin'):
        os.makedirs(os.path.join(root, d), exist_ok=True)
    if a_minimal:
        w('etc/dpkg/dpkg.cfg.d/excludes', '# no documentation or translations (mkrootfs.py)\n'
          + ''.join(f'path-exclude={g}\n' for g in EXCLUDE) + ''.join(f'path-include={g}\n' for g in INCLUDE))
    w('var/lib/dpkg/arch', f'{NATIVE}\n{OURS_ARCH}\n')
    for f in ('diversions', 'statoverride', 'available'):
        w(f'var/lib/dpkg/{f}', '')
    w('var/lib/dpkg/triggers/File', '')
    w('var/lib/dpkg/triggers/Unincorp', '')


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--pool', default=os.path.expanduser('~/gex-sandbox/scratch/CC/debian/pool'))
    ap.add_argument('--cache', default=os.path.expanduser('~/gex-sandbox/scratch/CC/deb'))
    ap.add_argument('--out', required=True)
    ap.add_argument('--tar', help='also write the tree as a tar (owner root)')
    ap.add_argument('--roots', default=','.join(ROOTS))
    ap.add_argument('--extra', default='', help='more arm64 packages (and their closure)')
    ap.add_argument('--exclude', default='', help='our packages to leave out of the image (still in the repo)')
    ap.add_argument('--full', action='store_true', help='keep documentation and translations')
    a = ap.parse_args()
    global a_minimal
    a_minimal = not a.full

    ours = []
    for deb in sorted(glob.glob(os.path.join(a.pool, f'*_{OURS_ARCH}.deb'))):
        ours.append((deb, control(deb)))
    excluded = set(filter(None, a.exclude.split(',')))
    image_ours = [(d, c) for d, c in ours if c['Package'] not in excluded]
    our_names = {c['Package'] for _, c in ours}
    for _, c in ours:
        for item in c.get('Provides', '').split(','):
            if item.strip():
                our_names.add(item.strip().split(' ')[0])
    # (a package of ours left out of the image does not satisfy anything)
    have_ours = {c['Package'] for _, c in image_ours}
    for _, c in image_ours:
        for item in c.get('Provides', '').split(','):
            if item.strip():
                have_ours.add(item.strip().split(' ')[0])

    pkgs, provides = debfetch.packages(a.cache, SUITE, NATIVE)
    roots = [r for r in a.roots.split(',') + a.extra.split(',') if r]
    # what ours depend on from Debian (architecture-independent data)
    for _, c in image_ours:
        for alts in debfetch.clauses(c):
            if not any(x in have_ours for x in alts):
                roots.append(alts[0])
    order = debfetch.closure(pkgs, provides, roots, exclude=our_names)
    # A name ours provides is never Debian's in the image.
    order = [n for n in order if n not in our_names]
    unmet = []
    have = set(order) | have_ours
    for n in order:
        for item in pkgs[n].get('Provides', '').split(','):
            if item.strip():
                have.add(item.strip().split(' ')[0])
    for p in [pkgs[n] for n in order] + [c for _, c in image_ours]:
        for alts in debfetch.clauses(p):
            if not any(x in have for x in alts):
                unmet.append(f'{p["Package"]} needs {" | ".join(alts)}')
    if unmet:
        sys.exit('the closure is not self-consistent:\n  ' + '\n  '.join(unmet))
    native = [(debfetch.fetch(a.cache, pkgs[n]), pkgs[n]) for n in order]
    print(f'{len(native)} Debian packages, {len(image_ours)} of ours', file=sys.stderr)

    root = a.out
    if os.path.exists(root):
        subprocess.run(['chmod', '-R', 'u+rwX', root], check=False)
        shutil.rmtree(root)
    os.makedirs(os.path.join(root, 'usr'))
    for d in ('bin', 'sbin', 'lib'):
        os.makedirs(os.path.join(root, 'usr', d), exist_ok=True)
        os.symlink(f'usr/{d}', os.path.join(root, d))
    info = os.path.join(root, 'var/lib/dpkg/info')
    os.makedirs(info)
    # the multiarch database layout (`pkg:arch.*` for Multi-Arch: same)
    open(os.path.join(info, 'format'), 'w').write('1\n')
    status = []
    for deb, idx in native + image_ours:
        ctl = control(deb)
        paths = unpack(deb, root)
        stem = info_name(ctl)
        open(os.path.join(info, f'{stem}.list'), 'w').write('\n'.join(paths) + '\n')
        sums = control_member(deb, 'md5sums')
        if sums:
            open(os.path.join(info, f'{stem}.md5sums'), 'w').write(sums)
        conf = control_member(deb, 'conffiles')
        cmd5 = []
        if conf:
            open(os.path.join(info, f'{stem}.conffiles'), 'w').write(conf)
            for p in conf.split():
                f = os.path.join(root, real_rel(root, p.lstrip('/')))
                if os.path.isfile(f):
                    cmd5.append((p, md5(f)))
        status.append(status_stanza(ctl, cmd5))
    open(os.path.join(root, 'var/lib/dpkg/status'), 'w').write('\n'.join(status))
    configure(root, ours, native)
    write_repo(root, ours)

    size = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(root) for f in fs
               if not os.path.islink(os.path.join(d, f)))
    print(f'{root}: {size / (1 << 20):.1f} MiB', file=sys.stderr)
    if a.tar:
        subprocess.run(['tar', '-C', root, '--owner=0', '--group=0', '--numeric-owner', '--sort=name',
                        '-cf', a.tar, '.'], check=True)
        print(f'{a.tar}: {os.path.getsize(a.tar) / (1 << 20):.1f} MiB', file=sys.stderr)


if __name__ == '__main__':
    main()
