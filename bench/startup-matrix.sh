#!/bin/sh
# Every app the project says works, two trees against each other, on V8.
#
#   usage: bench/startup-matrix.sh [reps] [tree-A] [tree-B]
#
# Each side uses its own gexc, its own browser shim and its own JS, so this
# compares the product rather than one binary -- point it at a worktree of the
# commit you want as a baseline and at your own tree. Both must be built
# (`cargo build --release` plus `shims/browser/build.sh`).
#
# `GEX_TIME_JIT` splits each run into total and the part spent translating, so
# startup and steady-state move separately. That split is the point: a change
# that makes the translator slower and the translated code faster reads as a
# win on any seconds-long benchmark and a loss on `ls`, and only this
# separation shows both.
#
# GEX_ROOT is a Debian arm64 rootfs (see tests/integration); the workloads it
# needs beyond stock Debian are created below if missing.
set -u
REPS="${1:-3}"
A="${2:-/tmp/gex-head}"
B="${3:-$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)}"
R="${GEX_ROOT:-/tmp/gex-debian/root}"
OUT="${GEX_MATRIX_OUT:-/tmp/matrix.tsv}"
: > "$OUT"

for t in "$A" "$B"; do
  [ -x "$t/target/release/gexc" ] || { echo "no gexc in $t -- cargo build --release" >&2; exit 1; }
  [ -f "$t/shims/browser/js/run-node.mjs" ] || { echo "no shim in $t" >&2; exit 1; }
done
[ -d "$R" ] || { echo "no rootfs at $R (set GEX_ROOT)" >&2; exit 1; }

# Stock Debian has no long-running workload in it, and a program that exits in
# 300ms is measuring startup only. These give bash and perl something to do.
[ -f "$R/var/corpus.txt" ] || cat "$R"/usr/share/doc/*/copyright "$R"/etc/* 2>/dev/null \
  | head -c 4000000 > "$R/var/corpus.txt"
[ -f "$R/var/corpus.bin" ] || head -c 4000000 "$R/usr/bin/perl" > "$R/var/corpus.bin"
[ -f "$R/gex-loop.sh" ] || cat > "$R/gex-loop.sh" <<'SH'
i=0; acc=0
while [ $i -lt 30000 ]; do acc=$((acc + i * 7 % 13)); i=$((i + 1)); done
echo $acc
SH
[ -f "$R/gex-bench.pl" ] || cat > "$R/gex-bench.pl" <<'PL'
my %h; my $acc = 0;
for my $i (1 .. 400000) { my $k = "key" . ($i * 2654435761 % 100000); $h{$k} += $i; $acc += length($k); }
my @ks = sort keys %h; for my $k (@ks) { $acc += $h{$k} % 97 }
print "n=", scalar(@ks), " acc=$acc\n";
PL

# One run, printing "total translating blocks".
run1() {  # root wasm -> "total translate install workerstart blocks"
  out=$(GEX_TIME_JIT=1 node "$1/shims/browser/js/run-node.mjs" \
    --dir "/=$R" --cwd / "$2" $ARGS 2>&1 | grep -m1 "pid 1: .*ms total")
  t=$(echo "$out"  | grep -oE "[0-9]+ms total" | grep -oE "^[0-9]+")
  tr_=$(echo "$out" | grep -oE "[0-9]+ms translating" | grep -oE "^[0-9]+")
  in_=$(echo "$out" | grep -oE "[0-9]+ms installing" | grep -oE "^[0-9]+")
  ws=$(echo "$out"  | grep -oE "worker start [0-9]+ms" | grep -oE "[0-9]+")
  bl=$(echo "$out"  | grep -oE "translating [0-9]+ blocks" | grep -oE "[0-9]+")
  echo "${t:-} ${tr_:-0} ${in_:-0} ${ws:-0} ${bl:-0}"
}

# Both sides, interleaved and alternating, minimum per side. Running all of one
# side and then all of the other lets the machine drift between them, which is
# worth several percent here -- `bench/ab.mjs` alternates for the same reason.
both() {
  ab=999999; abtr=0; abbl=0; bb=999999; bbtr=0; bbbl=0
  # Worst as well as best, so a row whose two sides overlap can be seen to.
  awst=0; bwst=0
  i=0
  while [ "$i" -lt "$REPS" ]; do
    if [ $((i % 2)) -eq 0 ]; then first=A; else first=B; fi
    for side in $([ "$first" = A ] && echo "A B" || echo "B A"); do
      if [ "$side" = A ]; then r=$(run1 "$A" /tmp/mx.a.wasm); else r=$(run1 "$B" /tmp/mx.b.wasm); fi
      set -- $r
      [ $# -lt 5 ] && continue
      [ -z "$1" ] && continue
      t=$1; tr_=$2; in_=$3; ws=$4; bl=$5
      # Startup is everything before the guest's own work: translating,
      # installing what was translated, and instantiating. Run is the rest.
      st=$((tr_ + in_ + ws)); rn=$((t - tr_ - in_))
      [ "$rn" -lt 0 ] && rn=0
      if [ "$side" = A ]; then
        [ "$t" -lt "$ab" ] && { ab=$t; abtr=$st; abbl=$rn; }
        [ "$t" -gt "$awst" ] && awst=$t
      else
        [ "$t" -lt "$bb" ] && { bb=$t; bbtr=$st; bbbl=$rn; }
        [ "$t" -gt "$bwst" ] && bwst=$t
      fi
    done
    i=$((i + 1))
  done
}

one() {  # name prog [args...]
  name="$1"; prog="$2"; shift 2
  ARGS="$prog $*"
  if [ ! -x "$prog" ]; then printf '%s\tskip\n' "$name" >> "$OUT"; echo "$name  skip"; return; fi
  "$A/target/release/gexc" build "$prog" -o /tmp/mx.a.wasm --no-shared-memory >/dev/null 2>&1 \
    || { printf '%s\tskip-buildA\n' "$name" >> "$OUT"; echo "$name  skip (A build)"; return; }
  "$B/target/release/gexc" build "$prog" -o /tmp/mx.b.wasm --no-shared-memory >/dev/null 2>&1 \
    || { printf '%s\tskip-buildB\n' "$name" >> "$OUT"; echo "$name  skip (B build)"; return; }
  both
  a="$ab $abtr $abbl"; b="$bb $bbtr $bbbl"
  sa=$(stat -c%s /tmp/mx.a.wasm); sb=$(stat -c%s /tmp/mx.b.wasm)
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "$a" "$b" "$sa" "$sb" "$awst" "$bwst" >> "$OUT"
  echo "$name  A=[$a]  B=[$b]"
}

one ls          $R/bin/ls -la /usr/bin
one sha256sum   $R/usr/bin/sha256sum /var/corpus.bin /var/corpus.bin /var/corpus.bin
one wc          $R/usr/bin/wc -w /var/corpus.txt
one grep        $R/bin/grep -c abc /var/corpus.txt
one sed         $R/bin/sed -e s/abc/xyz/g /var/corpus.txt
one sort        $R/usr/bin/sort -o /dev/null /var/corpus.txt
one gzip        $R/bin/gzip -k -f -9 /var/corpus.txt
one du          $R/usr/bin/du -s /usr
one find        $R/usr/bin/find /usr/bin -name a*
one tar         $R/bin/tar cf /dev/null /usr/bin
one dpkg        $R/usr/bin/dpkg --version
one bash        $R/bin/bash /gex-loop.sh
one perl        $R/usr/bin/perl /gex-bench.pl
one openssl     $R/usr/bin/openssl dgst -sha512 -r /var/corpus.bin /var/corpus.bin /var/corpus.bin
one apt-cache   $R/usr/bin/apt-cache --version
