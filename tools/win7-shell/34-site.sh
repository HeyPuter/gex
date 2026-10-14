#!/bin/bash
# 34: the site, $W7_OUT/site: `gexc eject` of the session (33) into the
# project $W7_OUT/project (the bundle, the units, the runtime from 20, the
# page from web/), with the compositor left out (Wine's windows are the
# page's own: the shell draws the desktop); user/eject.json from
# eject.json (the user W7_USER the prefix was made for, the page's size);
# the project's build; then web/eject-site.mjs (--strip --prune --dedupe).
# The site is static files: serve the directory over HTTPS (it needs
# SharedArrayBuffer: COOP/COEP headers, see docs/eject.md) and open
# index.html. It holds Windows 7's files: keep it private.
. "$(dirname "$0")/lib.sh"
G=$GEX_SRC/target/x86/release/gexc; P=$W7_OUT/project; SITE=$W7_OUT/site; L=$W7_OUT/logs; W7_USER=${W7_USER:-claude}
[ -f "$W7_OUT/rec/w3-session.json" ] || { echo "run 33-units.sh first" >&2; exit 2; }
command -v node > /dev/null || { echo "needs node" >&2; exit 2; }
step "eject (logs/site.log)"
chmod -R u+w "$P" "$SITE" 2>/dev/null || true; rm -rf "$P" "$SITE"
"$G" eject "$W7_OUT/rec" --out "$P" --web-src "$GEX_SRC/web" --unit-cache "$W7_OUT/units" --features -compositor > "$L/site.log" 2>&1 \
  || { tail -20 "$L/site.log"; exit 1; }
sed "s/@USER@/$W7_USER/" "$W7_TOOLS/eject.json" > "$P/user/eject.json"
step "build"
sh "$P/build.sh" >> "$L/site.log" 2>&1 || { tail -20 "$L/site.log"; exit 1; }
step "the site"
node "$GEX_SRC/web/eject-site.mjs" "$P" "$SITE" --strip --prune --dedupe --title "Windows 7" >> "$L/site.log" 2>&1 || { tail -20 "$L/site.log"; exit 1; }
sed -i 's|<title>gex-eject</title>|<title>Windows 7</title>|' "$SITE/index.html"
step "done: $SITE ($(du -sh "$SITE" | cut -f1))"
