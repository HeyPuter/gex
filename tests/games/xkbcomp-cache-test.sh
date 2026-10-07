#!/bin/bash
# tests/games/xkbcomp-cache.sh on the host, with a fake xkbcomp: the first
# keymap compiles, the same text again is a cache hit, a different keymap
# compiles once, and every output is the right one.
set -u
here=$(cd "$(dirname "$0")" && pwd)
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
mkdir "$t/bin"
cat > "$t/bin/xkbcomp" <<'X'
#!/bin/sh
for a; do out=$a; done; { echo compiled; cat; } > "$out"; echo x >> "$(dirname "$out")/calls"
X
chmod +x "$t/bin/xkbcomp"
sed "s|/usr/bin/xkbcomp.real|$t/bin/xkbcomp|" "$here/xkbcomp-cache.sh" > "$t/w.sh"
printf 'xkb_keymap {\n  a b\n}\n' > "$t/k1"
printf 'xkb_keymap {\n  a c\n}\n' > "$t/k2"
fail=0
for k in k1 k1 k2 k1 k2; do
    XKBCOMP_CACHE=$t/c sh "$t/w.sh" -w 0 -xkm - -em1 x "$t/out.xkm" < "$t/$k"
    if cmp -s "$t/out.xkm" <(echo compiled; cat "$t/$k"); then echo "ok   $k"; else echo "FAIL $k: wrong output"; fail=1; fi
done
n=$(wc -l < "$t/calls")
if [ "$n" = 2 ]; then echo "ok   2 real compiles for 5 calls"; else echo "FAIL $n real compiles, want 2"; fail=1; fi
exit $fail
