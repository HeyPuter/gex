#!/bin/bash
# A page that keeps /save in IndexedDB (app.js `persist`) keeps a save the
# program made a few seconds before the page was reloaded, with nothing
# asking for a snapshot and the program still running: the store snapshots
# soon after a save is finished (a close after a write, a rename), since a
# reload does not wait for the snapshot it starts. Through a symlink into
# /save, by a temporary file renamed over the save and by stdio over it.
# save-probe.c and persist-save.mjs.
#
#   tests/eject/persist-save.sh --build   on the build host (needs
#       web/public/gex with the x86 shim and kernel, and web/node_modules):
#       the probe, its bundle and an eject page around them, in
#       target/persist-save
#   tests/eject/persist-save.sh   runs it in headless Chromium (no GPU);
#       DELAY (ms, default 3000) between a save and the reload
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
T=$ROOT/target/persist-save
if [ "${1:-}" = --build ]; then
    rm -rf "$T"; mkdir -p "$T/stage/game/data" "$T/stage/save/data/SAVE" || exit 1
    cc -O2 -o "$T/stage/game/save-probe" "$ROOT/tests/eject/save-probe.c" || exit 1
    for f in $(ldd "$T/stage/game/save-probe" | grep -o '/[^ ]*'); do
        mkdir -p "$T/stage$(dirname "$f")" && cp -L "$f" "$T/stage$f" || exit 1
    done
    ln -s /save/data/SAVE "$T/stage/game/data/SAVE" || exit 1
    (cd "$T/stage" && tar -cf "$T/bundle.tar" --format=gnu *) || exit 1
    rm -rf "$T/stage"
    sh "$ROOT/web/build-eject-page.sh" "$T/site" > "$T/build.log" 2>&1 || { tail -5 "$T/build.log"; exit 1; }
    mv "$T/bundle.tar" "$T/site/bundle.tar"
    echo '{"base":"lazy/","files":[]}' > "$T/site/lazy.json"
    cp "$ROOT/tests/eject/persist-save.html" "$T/site/index.html"
    echo "built $T"
    exit 0
fi
[ -f "$T/site/bundle.tar" ] || { echo "FAIL no page: run $0 --build on the build host first"; exit 1; }
cd "$ROOT"
timeout 900 node tests/eject/persist-save.mjs "$T/site" "$@"
