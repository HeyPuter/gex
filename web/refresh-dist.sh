#!/bin/sh
# Keep web/dist from serving code that no longer exists.
#
# `web/public` is what the build scripts install into; `web/dist` is a *copy*
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

[ -d "$HERE/dist" ] || exit 0

if [ -d "$HERE/node_modules" ] && command -v npm >/dev/null 2>&1; then
    if (cd "$HERE" && npm run --silent build >/dev/null 2>&1); then
        echo "  web/dist: rebuilt"
        exit 0
    fi
    echo "  web/dist: vite build failed" >&2
fi

rm -rf "$HERE/dist"
echo "  web/dist: removed -- it was older than what was just installed." >&2
echo "  Run 'npm run build' in web/ to make it again." >&2
