#!/bin/bash
# Build corpus programs with gexcc and run their own test suites (the
# programs run under gexc through their containers' host launcher).
#
#   tests/gexcc/corpus/run.sh NAME...      (or `all`)
#
# GEXCC_PREFIX: an installed toolchain (tools/gexcc/install.sh)
# GEXCC_SRC:    the tarballs (fetch.sh)
# GEXCC_WORK:   build trees, logs/, results.txt
# JOBS:         make -j (default 6)
# KEEP=1:       reuse an unpacked tree (no fresh unpack)
# TESTONLY=1:   reuse the built tree, run the tests only
#
# Each recipe (recipes.sh) builds in the unpacked tree and its test step
# prints `RESULT pass=N fail=N skip=N`; results.txt keeps one line per
# program: name version build tests.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
: "${GEXCC_PREFIX:?set GEXCC_PREFIX}" "${GEXCC_SRC:?set GEXCC_SRC}" "${GEXCC_WORK:?set GEXCC_WORK}"
export PATH=$GEXCC_PREFIX/bin:$PATH
export SYSROOT=$GEXCC_PREFIX/sysroot T=wasm32-unknown-linux-musl JOBS=${JOBS:-6}
mkdir -p "$GEXCC_WORK/logs"
. "$HERE/recipes.sh"
names=("$@")
[ "${names[0]:-}" = all ] && names=($(grep -v '^#' "$HERE/sources.txt" | awk '{print $1}' | grep -v -- '-tests$'))
for name in "${names[@]}"; do
  line=$(grep "^$name " "$HERE/sources.txt") || { echo "no source for $name"; continue; }
  read -r _ ver url _ <<< "$line"
  tarball=$GEXCC_SRC/$(basename "$url")
  dir=$GEXCC_WORK/$name
  if [ -z "${KEEP:-}${TESTONLY:-}" ] || [ ! -d "$dir" ]; then
    rm -rf "$dir"; mkdir -p "$dir"
    tar xf "$tarball" -C "$dir" --strip-components=1 || { echo "FAIL unpack $name"; continue; }
  fi
  log=$GEXCC_WORK/logs/$name
  build=FAIL tests=-
  if [ -n "${TESTONLY:-}" ] || (cd "$dir" && set -e && "build_$name") > "$log.build.log" 2>&1; then
    build=ok
    if declare -F "test_$name" > /dev/null; then
      (cd "$dir" && "test_$name") > "$log.test.log" 2>&1
      r=$(grep -a '^RESULT ' "$log.test.log" | tail -1)
      if [ -n "$r" ]; then
        p=$(sed -n 's/.*pass=\([0-9]*\).*/\1/p' <<< "$r"); f=$(sed -n 's/.*fail=\([0-9]*\).*/\1/p' <<< "$r")
        s=$(sed -n 's/.*skip=\([0-9]*\).*/\1/p' <<< "$r")
        tests="$p/$((p + f)) pass, $s skip"
      else
        tests="no result"
      fi
    fi
  fi
  echo "$name $ver build=$build tests=$tests"
  touch "$GEXCC_WORK/results.txt"
  sed -i "/^$name /d" "$GEXCC_WORK/results.txt"
  echo "$name $ver build=$build tests=$tests" >> "$GEXCC_WORK/results.txt"
done
