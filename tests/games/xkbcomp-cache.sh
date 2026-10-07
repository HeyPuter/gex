#!/bin/sh
# A caching xkbcomp for Xwayland run inside gex (matrix.sh binds it over
# /usr/bin/xkbcomp with `gexc run --bind`, the real one at
# /usr/bin/xkbcomp.real). Xwayland compiles the compositor's keymap with
# xkbcomp at every start and for each keyboard focus; translated, on a loaded
# machine, that is minutes per run with X clients waiting. The keymap gex's
# compositor sends is the same every time, so the compiled .xkm is kept,
# keyed by the keymap text, and copied out on a match.
#
# Shell builtins only, but for the final cp: every process started under
# gex is translated afresh (a short one costs seconds of compilation), and
# the first version's id, mkdir, cat, cmp and rm made a cache hit six
# processes, which on a loaded gooner held X clients for minutes.
#
# Xwayland's call: xkbcomp -w N ... -xkm "-" -em1 ... -eml ... "DIR/NAME.xkm"
# (the keymap on stdin, the output file last).
cache=${XKBCOMP_CACHE:-/tmp/gex-xkm-cache-${USER:-${LOGNAME:-user}}}
for a; do out=$a; done
[ -d "$cache" ] || mkdir -p "$cache"
# Guest pids start low in every run, so these names recycle.
in=$cache/tmp.$$
while IFS= read -r l || [ -n "$l" ]; do printf '%s\n' "$l"; done > "$in"
# Same text, line for line, as $in? Both are written a whole line at a
# time (printf above, and the same for the kept copy), so plain reads do.
same() {
    {
        while IFS= read -r a <&3; do
            IFS= read -r b <&4 || return 1
            [ "$a" = "$b" ] || return 1
        done
        ! IFS= read -r b <&4
    } 3< "$1" 4< "$in"
}
for k in "$cache"/*.in; do
    [ -f "$k" ] || continue
    if same "$k"; then
        exec cp "${k%.in}.xkm" "$out"
    fi
done
# Not seen before: compile, then keep the input and the output.
/usr/bin/xkbcomp.real "$@" < "$in" || exit
n=0
for k in "$cache"/*.in; do [ -f "$k" ] && n=$((n + 1)); done
while IFS= read -r l || [ -n "$l" ]; do printf '%s\n' "$l"; done < "$in" > "$cache/$n.in.part"
cp "$out" "$cache/$n.xkm" && mv "$cache/$n.in.part" "$cache/$n.in"
