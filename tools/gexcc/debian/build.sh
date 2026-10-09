#!/bin/bash
# Build Debian source packages with gexcc into native wasm32 programs.
#
#   tools/gexcc/debian/build.sh SRCPKG...        (or a tier: tier1, tier2, ...)
#
# Environment:
#   GEXCC_PREFIX  an installed toolchain (tools/gexcc/install.sh); its sysroot
#                 receives the libraries later packages link
#   GEXCC_DEB     work area (default ~/gex-sandbox/scratch/CC/debian):
#                 src/ unpacked sources, stage/SRCPKG/ install trees,
#                 logs/, built.txt
#   JOBS          make -j (default 6)
#   KEEP=1        reuse an unpacked source tree
#
# Versions are pinned in packages.txt; debsrc.py fetches and verifies them
# and applies Debian's patches, then ours (patches/SRCPKG/). Each recipe
# (recipes.sh) builds in the source tree and installs into $DEST. Run builds
# in a resource scope (docs/gexcc.md §2): systemd-run --user --scope -p
# TasksMax=4096 -p MemoryMax=32G nice -n 19.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
: "${GEXCC_PREFIX:?set GEXCC_PREFIX}"
export GEXCC_DEB=${GEXCC_DEB:-$HOME/gex-sandbox/scratch/CC/debian}
export PATH=$GEXCC_PREFIX/bin:$PATH
export SYSROOT=$GEXCC_PREFIX/sysroot T=wasm32-unknown-linux-musl JOBS=${JOBS:-6}
# for every configure, including the reruns make starts when Debian's patches
# touched configure.ac or m4/ (config.status --recheck keeps the arguments,
# not the environment)
export CONFIG_SITE=$GEXCC_PREFIX/share/gexcc/config.site
# no Texinfo on the host, and the rootfs carries no info pages
export MAKEINFO=true
mkdir -p "$GEXCC_DEB"/{src,stage,logs}
. "$HERE/recipes.sh"

pin() { awk -v p="$1" '$1 == p {print $2}' "$HERE/packages.txt"; }
names=()
for n in "$@"; do
  case $n in
    tier*) names+=($(awk -v t="$n" '$3 == t {print $1}' "$HERE/packages.txt")) ;;
    *) names+=("$n") ;;
  esac
done
for name in "${names[@]}"; do
  ver=$(pin "$name")
  [ -n "$ver" ] || { echo "$name: not in packages.txt"; continue; }
  upver=${ver#*:}
  dir=$GEXCC_DEB/src/$name-$upver
  [ -z "${KEEP:-}" ] && rm -rf "$dir"
  python3 -I "$HERE/debsrc.py" --into "$GEXCC_DEB/src" "$name=$ver" > /dev/null || { echo "FAIL fetch $name"; continue; }
  export DEST=$GEXCC_DEB/stage/$name
  rm -rf "$DEST"; mkdir -p "$DEST"
  log=$GEXCC_DEB/logs/$name.log
  # (set -e is ignored inside a function called from an && list or an if
  # condition, even in a subshell: run it as a plain command)
  (cd "$dir" || exit 1; set -e; "build_${name//[-.+]/_}") > "$log" 2>&1
  if [ $? = 0 ]; then r=ok; else r=FAIL; fi
  echo "$name $ver build=$r"
  touch "$GEXCC_DEB/built.txt"; sed -i "/^$name /d" "$GEXCC_DEB/built.txt"
  echo "$name $ver build=$r" >> "$GEXCC_DEB/built.txt"
done
