#!/bin/sh
# A caching xkbcomp for Xwayland run inside gex (matrix.sh binds it over
# /usr/bin/xkbcomp with `gexc run --bind`, the real one at
# /usr/bin/xkbcomp.real). Xwayland compiles the compositor's keymap with
# xkbcomp at every start and for each keyboard focus; translated, on a loaded
# machine, that is minutes per run with X clients waiting. The keymap gex's
# compositor sends is the same every time, so the compiled .xkm is kept,
# keyed by the keymap text, and copied out on a match.
#
# Xwayland's call: xkbcomp -w N ... -xkm "-" -em1 ... -eml ... "DIR/NAME.xkm"
# (the keymap on stdin, the output file last).
cache=${XKBCOMP_CACHE:-/tmp/gex-xkm-cache-$(id -u)}
for a; do out=$a; done
mkdir -p "$cache"
in=$cache/in.$$
cat > "$in"
for k in "$cache"/*.in; do
    [ -f "$k" ] || continue
    if cmp -s "$k" "$in"; then
        rm -f "$in"
        exec cp "${k%.in}.xkm" "$out"
    fi
done
# Not seen before: compile, then keep the input and the output.
/usr/bin/xkbcomp.real "$@" < "$in" || { rc=$?; rm -f "$in"; exit $rc; }
n=$(ls "$cache" | grep -c '\.in$')
mv "$in" "$cache/$n.in" && cp "$out" "$cache/$n.xkm"
