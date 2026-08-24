#!/bin/sh
# Build a minimal Debian arm64 root filesystem, without root and without
# debootstrap.
#
#   web/build-debian.sh [out.tar] [work-dir]
#
# debootstrap needs root because it wants to chroot and run maintainer
# scripts. Nothing here does: a .deb is an `ar` archive with a tar inside, and
# unpacking the dependency closure of `apt` into a directory produces a tree
# that is a working Debian userspace for everything except the parts that
# needed those scripts to run. `dpkg --configure` can finish the job from
# inside the guest, which is where it belongs anyway.
#
# What this leaves out on purpose: /var/lib/dpkg/status is written from the
# packages' own control files, so dpkg believes what is installed is
# installed, and apt will not try to fetch it again.
set -eu

OUT="${1:-$(cd "$(dirname "$0")" && pwd)/public/rootfs/debian.tar}"
WORK="${2:-${TMPDIR:-/tmp}/gex-debian}"
MIRROR="${GEX_DEBIAN_MIRROR:-http://deb.debian.org/debian}"
SUITE="${GEX_DEBIAN_SUITE:-bookworm}"
ARCH=arm64

# The roots of the closure. `apt` is the point of the exercise; the rest is
# what a shell prompt needs to be worth having.
# libc-bin is not a dependency of libc6 but dpkg refuses to run without the
# ldconfig in it; perl-base is what maintainer scripts are written in.
# Overridable, because the same closure-resolver builds the graphics rootfs
# the virtio-gpu tests need -- that one wants Mesa and mesa-utils-bin on top,
# and there is no reason for a second copy of this script to say so.
#
# `ncurses-base` is not optional and its absence was invisible. It ships
# `/lib/terminfo`, and without a terminfo database `TERM=xterm-256color` names
# a terminal nothing can describe: `clear`, `tput`, `less`, `top` and every
# other curses program either refuse to start or fall back to `dumb` and
# scroll. The rootfs had no terminfo file of any kind, and `TERM` was being
# set to a value nothing in it could read.
#
# `usr-is-merged` is here to be *preferred*, not because anything wants it:
# `init-system-helpers` depends on `usrmerge | usr-is-merged`, and the resolver
# takes an alternative already in the closure over the first one. Without it the
# first one wins -- `usrmerge`, which converts a system to merged /usr and pulls
# perl in behind it -- and the recorded system then has an unmet `perl:any`,
# which is enough for apt to refuse every `install`. This tree is unpacked
# merged, so `usr-is-merged` is the true answer as well as the small one.
ROOTS="${GEX_DEBIAN_ROOTS:-apt dash bash coreutils dpkg findutils grep sed tar gzip diffutils \
       debianutils base-files base-passwd hostname ca-certificates \
       ncurses-base ncurses-bin less procps usr-is-merged \
       libc-bin perl-base debian-archive-keyring}"

mkdir -p "$WORK/debs" "$WORK/root"
R="$WORK/root"

if [ ! -f "$WORK/Packages" ]; then
    echo "fetching the package index..."
    curl -fsSL "$MIRROR/dists/$SUITE/main/binary-$ARCH/Packages.gz" \
        -o "$WORK/Packages.gz"
    gunzip -kf "$WORK/Packages.gz"
fi

echo "resolving dependencies..."
GEX_ROOTS="$ROOTS" python3 - "$WORK" <<'PYEOF'
import os, sys, json
work = sys.argv[1]

# The index is stanzas of "Field: value", blank-line separated.
pkgs, provides, cur = {}, {}, {}
for line in open(f'{work}/Packages', encoding='utf-8', errors='replace'):
    if not line.strip():
        if cur.get('Package'):
            pkgs[cur['Package']] = cur
        cur = {}
    elif line[0] not in ' \t':
        k, _, v = line.partition(': ')
        cur[k] = v.strip()
if cur.get('Package'):
    pkgs[cur['Package']] = cur

# A virtual package is satisfied by whatever provides it. First one wins,
# which is what apt would do without a preference to go on.
for name, p in pkgs.items():
    for item in p.get('Provides', '').split(','):
        v = item.strip().split()[0] if item.strip() else ''
        if v and v not in provides:
            provides[v] = name

def base(name):
    """A dependency name without its architecture qualifier.

    A dependency may be written `perl:any` -- satisfied by that package from
    any architecture. The qualifier is not part of the package's name, so
    looking one up with it attached finds nothing, and `perl:any` was reported
    as a package the index does not have. It has `perl`.
    """
    return name.split(':', 1)[0]


def clauses(p):
    """The alternative sets of every hard dependency, in order.

    A clause is "a | b": alternatives, best first. Recommends are skipped --
    they are what turns a 40 MB closure into 400 MB.
    """
    out = []
    for field in ('Pre-Depends', 'Depends'):
        for clause in p.get(field, '').split(','):
            clause = clause.strip()
            if not clause:
                continue
            alts = [a.strip().split()[0] for a in clause.split('|') if a.strip()]
            if alts:
                out.append(alts)
    return out


def deps(p, chosen):
    """One name per clause: an alternative already chosen, else the first.

    Preferring what is already in the closure is what apt does, and it is not
    a nicety here. `init-system-helpers` depends on `usrmerge | usr-is-merged`,
    and taking the first alternative pulled in `usrmerge` -- the program that
    *converts* a system to merged /usr -- and behind it
    `libfile-find-rule-perl` and a dependency on `perl:any` that this closure
    does not carry. The tree unpacked here is merged from the start, so the
    right alternative is `usr-is-merged` (which is a root below, and which
    debootstrap picks for bookworm too).

    The cost of getting it wrong was not a bigger image, it was a broken one:
    `/var/lib/dpkg/status` recorded `usrmerge` as installed with an unmet
    dependency, so `apt-get install anything` refused to do any work at all
    with "Unmet dependencies. Try 'apt --fix-broken install'".
    """
    out = []
    for alts in clauses(p):
        pick = next((a for a in alts if base(a) in chosen), alts[0])
        out.append(pick)
    return out

seen, queue, missing = set(), os.environ['GEX_ROOTS'].split(), []
order = []
# Every root counts as chosen before anything transitive is resolved, so a
# clause naming one prefers it. The queue is FIFO and the roots go in first,
# which is what makes that true for the packages pulled in behind them.
chosen = {base(n) for n in queue}
while queue:
    name = base(queue.pop(0))
    if name in seen:
        continue
    seen.add(name)
    p = pkgs.get(name) or pkgs.get(provides.get(name, ''))
    if not p:
        missing.append(name)
        continue
    order.append(p)
    chosen.add(p['Package'])
    queue.extend(deps(p, chosen))

print(f'  {len(order)} packages', file=sys.stderr)
if missing:
    print(f'  not in the index, skipped: {" ".join(sorted(set(missing)))}',
          file=sys.stderr)

# Is what this is about to record as installed a system apt will work on?
#
# The status file below copies each package's own `Depends`, so a dependency the
# closure does not carry is written down as unmet -- and apt answers every
# `install` with "Unmet dependencies. Try 'apt --fix-broken install'" and does
# nothing at all. That was true of this image for as long as it has existed and
# the only symptom was in a browser, two hundred megabytes of rootfs later. It
# is one pass over the closure here.
have = {p['Package'] for p in order}
for p in order:
    for item in p.get('Provides', '').split(','):
        v = item.strip().split()[0] if item.strip() else ''
        if v:
            have.add(v)
unmet = []
for p in order:
    for alts in clauses(p):
        if not any(base(a) in have for a in alts):
            unmet.append(f"{p['Package']} needs {' | '.join(alts)}")
if unmet:
    print('  the closure is not self-consistent -- apt will refuse to install '
          'anything into it:', file=sys.stderr)
    for line in unmet:
        print(f'    {line}', file=sys.stderr)
    sys.exit(1)

total = sum(int(p.get('Size', 0)) for p in order)
print(f'  {total / (1 << 20):.0f} MiB of .debs', file=sys.stderr)
with open(f'{work}/closure.json', 'w') as f:
    json.dump([{'Package': p['Package'], 'Filename': p['Filename'],
                'Version': p.get('Version', ''),
                'Architecture': p.get('Architecture', ''),
                'Depends': p.get('Depends', ''),
                'Description': p.get('Description', '')} for p in order], f)
PYEOF

echo "fetching packages..."
python3 -c "
import json,sys
for p in json.load(open('$WORK/closure.json')): print(p['Filename'])
" | while read -r f; do
    base=$(basename "$f")
    [ -f "$WORK/debs/$base" ] || curl -fsSL "$MIRROR/$f" -o "$WORK/debs/$base"
done

echo "unpacking..."
rm -rf "$R"
# Every directory apt and dpkg expect to find, whether or not anything in the
# closure puts a file in one. apt does not create these and says so in a way
# that reads like a broken rootfs rather than a missing directory:
#
#   E: Archives directory /var/cache/apt/archives/partial is missing.
#   W: Unable to read /etc/apt/preferences.d/ - DirectoryExists (2: ...)
#
# `preferences.d` was never made at all; the rest were made here and then lost
# on the way into the guest, because an empty directory has no file to carry
# it. See `gex_add_dir`.
mkdir -p "$R/var/lib/dpkg/info" "$R/var/lib/dpkg/updates" "$R/var/lib/dpkg/triggers" \
         "$R/etc/apt/sources.list.d" "$R/etc/apt/preferences.d" "$R/etc/apt/apt.conf.d" \
         "$R/etc/apt/trusted.gpg.d" "$R/etc/apt/keyrings" "$R/etc/dpkg/dpkg.cfg.d" \
         "$R/var/cache/apt/archives/partial" "$R/var/cache/debconf" \
         "$R/var/lib/apt/lists/partial" "$R/var/lib/apt/periodic" \
         "$R/var/log/apt" "$R/var/log" "$R/tmp" "$R/proc" "$R/dev" "$R/run" \
         "$R/var/tmp" "$R/usr/share/keyrings"
for d in "$WORK"/debs/*.deb; do
    (cd "$WORK" && ar x "$d" && tar -C "$R" -xf data.tar.* && rm -f data.tar.* control.tar.* debian-binary)
done

# One archive kept whole, as a benchmark rather than as a package.
#
# 2.4 MB of xz that decompresses to 10.4 MB: enough guest CPU to measure,
# and the same library `apt-get update` spends its time in. `site.mjs
# debian-lzma` times it. Every other Debian case in that harness finishes
# in milliseconds and measures correctness; this one measures speed.
cp "$(ls "$WORK"/debs/libc6_*.deb | head -1)" "$R/var/libc6.deb"

echo "writing dpkg's idea of what is installed..."
python3 - "$WORK" "$R" <<'PYEOF'
import json, sys
work, root = sys.argv[1], sys.argv[2]
out = []
for p in json.load(open(f'{work}/closure.json')):
    out.append(
        f"Package: {p['Package']}\n"
        f"Status: install ok installed\n"
        # dpkg warns on every package without one, which is a hundred lines
        # of noise before anything useful happens.
        f"Maintainer: gex <root@localhost>\n"
        f"Architecture: {p['Architecture']}\n"
        f"Version: {p['Version']}\n"
        + (f"Depends: {p['Depends']}\n" if p['Depends'] else '')
        + f"Description: {p['Description'] or p['Package']}\n"
    )
open(f'{root}/var/lib/dpkg/status', 'w').write('\n'.join(out))
open(f'{root}/var/lib/dpkg/available', 'w').write('')
PYEOF

cat > "$R/etc/apt/sources.list" <<EOF
deb $MIRROR $SUITE main
EOF
# The account database, which no package installs.
#
# `base-passwd` ships `passwd.master` and `group.master` and its postinst
# copies them into place -- and nothing here runs maintainer scripts, so a
# rootfs built this way has no `/etc/passwd` at all. `ls -l` then prints bare
# numbers, `id` has no names for them, and anything that looks up a user by
# name fails in a way that reads as a missing account rather than a missing
# file.
if [ -f "$R/usr/share/base-passwd/passwd.master" ]; then
    cp "$R/usr/share/base-passwd/passwd.master" "$R/etc/passwd"
    cp "$R/usr/share/base-passwd/group.master" "$R/etc/group"
else
    printf 'root:x:0:0:root:/root:/bin/bash\n' > "$R/etc/passwd"
    printf 'root:x:0:\n' > "$R/etc/group"
fi
# The shadow file the passwd entries point at, with no password set: nothing
# here authenticates, and its absence is what makes `su` and `chpasswd` fail
# rather than refuse.
awk -F: '{ print $1 ":*:19000:0:99999:7:::" }' "$R/etc/passwd" > "$R/etc/shadow"
chmod 640 "$R/etc/shadow"
cat > "$R/etc/resolv.conf" <<'EOF'
nameserver 1.1.1.1
EOF
cat > "$R/etc/hostname" <<'EOF'
gex
EOF
cat > "$R/etc/hosts" <<'EOF'
127.0.0.1 localhost gex
EOF
# apt over plain HTTP, and without the sandbox user it cannot drop to.
# The directory is made rather than assumed: the same closure-resolver builds
# root filesystems that have no apt in them at all, and one of those should
# not fail on the last line of the script.
mkdir -p "$R/etc/apt/apt.conf.d"
cat > "$R/etc/apt/apt.conf.d/99gex" <<'EOF'
APT::Sandbox::User "root";
// Bookworm's arm64 index is 49 MB of Packages and 32 MB of Translation-en,
// and apt builds its cache in one mapping. Started at the default it grows
// to about 27 MB and then gives up:
//
//   E: Dynamic MMap ran out of room. Please increase the size of
//      APT::Cache-Start. Current value: 28311552.
//   E: Problem with MergeList .../binary-arm64_Packages
//
// So it has to be raised -- but only to what the index needs, not to a round
// number well past it. There is no copy-on-write here: one guest process is one
// linear memory, so `fork` copies the parent's mappings into the child, and
// every byte copied is a byte of the tab's memory. apt forks about 120 times in
// an `apt-get install` and keeps a dozen children alive at once, none of which
// touches this cache -- they exec a method or dpkg immediately. At 128 MiB that
// one mapping was most of the ~5 GB the renderer held, and the run died of it.
// 64 MiB parses the whole bookworm arm64 index with room to spare.
APT::Cache-Start "67108864";
Acquire::AllowInsecureRepositories "true";
Acquire::Check-Valid-Until "false";
APT::Get::AllowUnauthenticated "true";
Acquire::https::Verify-Peer "false";
Dpkg::Use-Pty "false";
EOF
mkdir -p "$R/root"
cat > "$R/root/.profile" <<'EOF'
export PS1='\u@gex:\w\$ '
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export DEBIAN_FRONTEND=noninteractive
EOF

mkdir -p "$(dirname "$OUT")"
tar -C "$R" -cf "$OUT" .
echo "$OUT: $(du -h "$OUT" | cut -f1)"
