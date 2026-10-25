#!/bin/bash
# The single-thread build's smoke leg (docs/single-thread.md §9):
#  1. tests/single-callout: blocking inside a nested call-out, under node, in
#     single mode and in the SAB build.
#  2. The page smoke test (web/test/site.mjs, the same demos as the default
#     leg) served without COOP/COEP. That page has no SharedArrayBuffer, so it
#     must run the single build. Each demo must pass, and the terminal must
#     say it was the single build, so a silent fallback cannot pass.
#  3. tests/single/app-gl.mjs: GL through app.js (nt.html, single=1), which
#     the demo page does not use; its Mesa is web/public/gex/st/.
#
#   tests/single/run.sh [gexc]          # demos: SP_DEMOS (default busybox native-debian gears)
#
# Needs web/build-all.sh --sp (or --both) first: web/dist-sp, served
# without COOP/COEP from its own dist. The gears demo also needs a
# WebGPU adapter; on a host without a GPU set GEX_WEBGPU_SOFTWARE=1.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
fail=0

"$root/tests/single-callout/run.sh" "$@" || fail=1

out=${WORK:-$root/target/single-test}
mkdir -p "$out"
[ -d "$root/web/dist-sp" ] || { echo "FAIL single-page (no web/dist-sp: run web/build-all.sh --sp)"; exit 1; }
for d in ${SP_DEMOS:-busybox native-debian gears}; do
	(cd "$root" && GEX_DIST="$root/web/dist-sp" GEX_NO_COI=1 GEX_TAIL=0 GEX_STDERR=all node web/test/site.mjs "$d") > "$out/site-$d.txt" 2>&1; rc=$?
	if [ $rc = 0 ] && grep -aq 'single-thread build (no SharedArrayBuffer)' "$out/site-$d.txt"; then
		echo "ok   single-page $d"
	else
		echo "FAIL single-page $d (rc=$rc; $out/site-$d.txt)"
		grep -a '^FAIL\|single-thread' "$out/site-$d.txt" | head -3
		fail=1
	fi
done
(cd "$root" && node tests/single/app-gl.mjs) > "$out/app-gl.txt" 2>&1; rc=$?
if [ $rc = 0 ]; then echo "ok   single-page app-gl"
else
	echo "FAIL single-page app-gl (rc=$rc; $out/app-gl.txt)"
	grep -a '^FAIL' "$out/app-gl.txt" | head -3
	fail=1
fi
exit $fail
