#!/bin/sh
# Keep page/dist from serving code that no longer exists.
#
# `page/public` is what the build scripts install into; `page/dist` is a *copy*
# of it plus the bundled JavaScript, made by vite. Nothing connects the two,
# so rebuilding the shim and reloading the page ran the previous build -- and
# the page says nothing, it just behaves like the old code. That cost two
# rounds of "the demo is still slow" against a shim that was already fixed,
# and before that a boot failure against a shim that had already been
# rebuilt.
#
# So every script that writes into public/ ends by calling this. If vite can
# run, dist is rebuilt. If it cannot, dist is *removed*, because a missing
# site is an obvious failure and a stale one is not.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"

# page/build.sh rebuilds the dists itself once its steps are done.
[ -z "${GEX_NO_REFRESH_DIST:-}" ] || exit 0

# One dist per flavour (page/build.sh): page/dist (MP) and page/dist-sp (SP).
# Each that exists is rebuilt from page/public the way build-all.sh makes it.
flavours=""
[ -d "$HERE/dist" ] && flavours="$flavours mp"
[ -d "$HERE/dist-sp" ] && flavours="$flavours sp"
[ -n "$flavours" ] || exit 0

if [ -d "$HERE/node_modules" ] && command -v npm >/dev/null 2>&1; then
    ok=1
    for f in $flavours; do
        "$HERE/build.sh" --$f --dist-only >/dev/null 2>&1 || ok=0
    done
    if [ $ok = 1 ]; then
        echo "  page/dist*: rebuilt ($flavours )"
        exit 0
    fi
    echo "  page/dist*: build failed" >&2
fi

rm -rf "$HERE/dist" "$HERE/dist-sp"
echo "  page/dist, page/dist-sp: removed -- they were older than what was just installed." >&2
echo "  Run page/build.sh to make them again." >&2
