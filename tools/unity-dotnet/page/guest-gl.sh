#!/bin/bash
# guest-gl.sh TREE: print the directory of the x86-64 guest GL (libGL.so.1, libEGL.so.1,
# libGLESv2.so.2) that the page runs of TREE should load, building it if need be.
#
# The guest GL is built from TREE's own sources (guest/gl/build.sh with GEX_CC=gcc) into
# TREE/target/guest-gl-x64, and rebuilt whenever guest/gl or crates/gex-gl/src/proto.rs is
# newer than it. A prebuilt guest GL goes stale silently: muck-page.sh defaulted to
# scratch/unity/gl64-new (2026-10-07), which has neither the frame-query cache nor the async
# present at swap interval 0 (85c3bb78, 7605de5c), and page Muck ran at 13.5 presents/s on
# it against 23.5 with a current build on the same tree (edger, 2026-10-11).
#
# Build it on the build host before syncing the tree to a bench box; a box that cannot build
# it (no gcc, or no third_party/opengl-registry checkout) falls back to GL64_FALLBACK
# (default scratch/unity/gl64q2, which is as fast as a master build: 21.5 vs 21.6) and says so
# on stderr.
set -u
TREE=$(realpath "${1:?guest-gl.sh TREE}")
OUT=$TREE/target/guest-gl-x64
FALLBACK=${GL64_FALLBACK:-$HOME/gex-sandbox/scratch/unity/gl64q2}
stale() {
	[ -f "$OUT/libGL.so.1" ] || return 0
	[ -n "$(find "$TREE/guest/gl" "$TREE/crates/gex-gl/src/proto.rs" -newer "$OUT/libGL.so.1" \
		\( -name '*.c' -o -name '*.h' -o -name '*.inc' -o -name '*.sh' -o -name '*.rs' \) \
		-not -path '*/out/*' 2>/dev/null | head -1)" ]
}
if stale; then
	if command -v gcc >/dev/null && [ -d "$TREE/third_party/opengl-registry/api" ] \
		&& (cd "$TREE" && GEX_CC=gcc sh guest/gl/build.sh "$OUT" >&2); then
		echo "guest-gl: built $OUT from $TREE" >&2
	elif [ -f "$OUT/libGL.so.1" ]; then
		echo "guest-gl: WARNING: $OUT is older than its sources and cannot be rebuilt here; using it" >&2
	else
		echo "guest-gl: WARNING: no guest GL build in $TREE; using the prebuilt $FALLBACK" >&2
		OUT=$FALLBACK
	fi
fi
echo "$OUT"
