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
ROOTS="apt dash bash coreutils dpkg findutils grep sed tar gzip diffutils \
       debianutils base-files base-passwd hostname ca-certificates \
       libc-bin perl-base"

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

def deps(p):
    """Every hard dependency, taking the first alternative of each set.

    Alternatives are "a | b", and the first is the maintainer's preference.
    Recommends are skipped: they are what turns a 40 MB closure into 400 MB.
    """
    out = []
    for field in ('Pre-Depends', 'Depends'):
        for clause in p.get(field, '').split(','):
            clause = clause.strip()
            if not clause:
                continue
            first = clause.split('|')[0].strip()
            name = first.split()[0] if first else ''
            if name:
                out.append(name)
    return out

seen, queue, missing = set(), os.environ['GEX_ROOTS'].split(), []
order = []
while queue:
    name = queue.pop(0)
    if name in seen:
        continue
    seen.add(name)
    p = pkgs.get(name) or pkgs.get(provides.get(name, ''))
    if not p:
        missing.append(name)
        continue
    order.append(p)
    queue.extend(deps(p))

print(f'  {len(order)} packages', file=sys.stderr)
if missing:
    print(f'  not in the index, skipped: {" ".join(sorted(set(missing)))}',
          file=sys.stderr)
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
mkdir -p "$R/var/lib/dpkg/info" "$R/etc/apt/sources.list.d" "$R/var/cache/apt/archives/partial" \
         "$R/var/lib/apt/lists/partial" "$R/var/log" "$R/tmp" "$R/proc" "$R/dev" "$R/run"
for d in "$WORK"/debs/*.deb; do
    (cd "$WORK" && ar x "$d" && tar -C "$R" -xf data.tar.* && rm -f data.tar.* control.tar.* debian-binary)
done

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
cat > "$R/etc/apt/apt.conf.d/99gex" <<'EOF'
APT::Sandbox::User "root";
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
