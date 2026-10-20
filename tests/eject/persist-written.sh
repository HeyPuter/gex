#!/bin/bash
# A page that keeps /save in IndexedDB (app.js `persist`) stores what the
# program wrote there and nothing else: not a bundle file it left alone, not
# a lazy file's contents (a snapshot used to read every file under its root,
# so a root over a lazy bundle split -- FL Studio's /pfx -- fetched it all and
# trapped), and it keeps a file restored from an earlier visit that the
# program did not write again. persist-probe.c and persist-written.mjs.
#
#   tests/eject/persist-written.sh --build   on the build host (needs
#       web/public/gex with the x86 shim and kernel, and web/node_modules):
#       the probe, its bundle and lazy file, and an eject page around them, in
#       target/persist-written
#   tests/eject/persist-written.sh   runs it in headless Chromium (no GPU)
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
T=$ROOT/target/persist-written
if [ "${1:-}" = --build ]; then
    rm -rf "$T"; mkdir -p "$T/stage/game" "$T/stage/save" || exit 1
    cc -O2 -o "$T/stage/game/persist-probe" "$ROOT/tests/eject/persist-probe.c" || exit 1
    for f in $(ldd "$T/stage/game/persist-probe" | grep -o '/[^ ]*'); do
        mkdir -p "$T/stage$(dirname "$f")" && cp -L "$f" "$T/stage$f" || exit 1
    done
    printf seed > "$T/stage/save/seed.txt"
    (cd "$T/stage" && tar -cf "$T/bundle.tar" --format=gnu *) || exit 1
    rm -rf "$T/stage"
    sh "$ROOT/web/build-eject-page.sh" "$T/site" > "$T/build.log" 2>&1 || { tail -5 "$T/build.log"; exit 1; }
    mv "$T/bundle.tar" "$T/site/bundle.tar"
    mkdir -p "$T/site/lazy"
    head -c 4194304 /dev/zero | tr '\0' 'z' > "$T/site/lazy/big"
    echo '{"base":"lazy/","files":[["/save/big.bin",4194304,"big",1700000000]]}' > "$T/site/lazy.json"
    cp "$ROOT/tests/eject/persist-written.html" "$T/site/index.html"
    echo "built $T"
    exit 0
fi
[ -f "$T/site/bundle.tar" ] || { echo "FAIL no page: run $0 --build on the build host first"; exit 1; }
cd "$ROOT"
timeout 600 node tests/eject/persist-written.mjs "$T/site" "$@"
