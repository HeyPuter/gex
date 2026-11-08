#!/usr/bin/env bash
# Which of dist.sh's prefixes were made for a Wine install:
#
#   wine/prefix-key.sh DIST                 DIST's key
#   wine/prefix-key.sh DIST PFX...          each PFX made for DIST, one a line
#   wine/prefix-key.sh --add PFX DIST       PFX made for DIST (dist.sh)
#
# A key is DIST's wine.inf stamp and its builtins, hashed: two installs of
# one Wine checkout share the stamp (wine.inf's mtime) but not their
# builtins, as one with winegstreamer and one without. A prefix keeps the
# keys of the installs it was made for in .gex-dists, one a line; one
# without that file predates it and is made for none.
set -euo pipefail
key() {
	[ -f "$1/share/wine/wine.inf" ] || { echo "$1 has no share/wine/wine.inf" >&2; return 2; }
	{ stat -L -c %Y "$1/share/wine/wine.inf"
	  local a; for a in x86_64 i386; do
		[ -d "$1/lib/wine/$a-windows" ] || continue
		ls "$1/lib/wine/$a-windows" | grep -Ev '\.(a|so|wasm|wasmref|m64)$' | sed "s/^/$a /"
	  done | LC_ALL=C sort -u; } | sha256sum | cut -c1-16
}
if [ "${1:-}" = --add ]; then
	[ $# = 3 ] || { sed -n 2,7p "$0" >&2; exit 2; }
	[ -f "$2/.gex-dists" ] || { echo "$2 has no .gex-dists: it predates the keys, so make it again" >&2; exit 1; }
	k=$(key "$3")
	grep -qx "$k" "$2/.gex-dists" || echo "$k" >> "$2/.gex-dists"
	exit 0
fi
[ $# -ge 1 ] || { sed -n 2,7p "$0" >&2; exit 2; }
k=$(key "$1"); shift
[ $# -gt 0 ] || { echo "$k"; exit 0; }
for p; do grep -qsx "$k" "$p/.gex-dists" && echo "$p"; done
exit 0
