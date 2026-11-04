#!/bin/bash
# The function-tier units of a frozen set of images, for one gexc: prints the
# cache directory (GEX_CGEN_CACHE) that holds them, building it first when no
# set exists for that gexc's unit-key inputs. Gate legs that run frozen Wine
# and game bundles on units (a game repo's page leg) call it, so a gex
# change that moves every unit key (gex-cgen's sources, gex-ld's through the
# recovery inputs, the ABI) costs one rebuild inside the leg, not a hand step.
#
#   scripts/units-for.sh DIR GEXC
#
# DIR holds the frozen inputs:
#   units.list  one image per line, "FILE BASE": the image (a path relative to
#               DIR, or absolute) and the hex base it is mapped at in the runs.
#               A hard-linked copy under DIR/images keeps them frozen.
#   icall/      optional: an indirect-call profile per image identity (the
#               same image at the same base, whatever the gexc), as
#               a game's own tooling places it. Each set is built
#               with it (devirtualised hot table calls: the "second pass").
# and the sets, DIR/ukeys/ID (ID: `gexc cgen-image --inputs`, a hash of
# everything a unit key takes besides its image), each with its build log at
# DIR/ukeys/ID.log. A set is complete once DIR/ukeys/ID/.complete exists; a
# build holds DIR/ukeys/.lock, so legs sharing DIR wait for one build. The
# three most recently used sets are kept; older ones are removed.
#
# Besides the images, each set holds the i386 trampoline page's unit
# (`gexc cgen-image --nt-trampolines32`).
#
# Stdout: the set's directory. Stderr: one line when a set is built ("units:
# built ...", with the time and the log), or the failure. Exit 1 on failure:
# a GEXC that is not an x86 gexc (no wine_libm provider), an image that does
# not build, or no units.list.
#
# Environment: UNITS_JOBS (default 12) jobs per image; the flavour switches a
# unit build takes (GEX_CGEN_GUARD=1 and so on) are passed through and are
# part of ID.
set -u
D=$(cd "${1:?usage: units-for.sh DIR GEXC}" && pwd)
G=$(realpath "${2:?usage: units-for.sh DIR GEXC}")
fail() { echo "units-for: $*" >&2; exit 1; }
[ -f "$D/units.list" ] || fail "no $D/units.list"
[ -x "$G" ] || fail "no gexc at $G"
K=$("$G" cgen-image --inputs 2>/dev/null) && [ -n "$K" ] || fail "$G cgen-image --inputs gave no id (a gexc older than --inputs?)"
mkdir -p "$D/ukeys"
U=$D/ukeys/$K
exec 9> "$D/ukeys/.lock"
flock 9
if [ ! -f "$U/.complete" ]; then
	t0=$(date +%s); log=$D/ukeys/$K.log
	rm -rf "$U.new"; mkdir -p "$U.new"
	[ -d "$D/icall" ] && cp -al "$D/icall" "$U.new/icall"
	run() { GEX_CGEN_CACHE="$U.new" GEX_CGEN_ICALL_COUNT=0 systemd-run --user --scope -q -p TasksMax=4096 -p MemoryMax=32G nice -n 19 "$G" cgen-image "$@"; }
	{
		echo "== units for inputs $K, gexc $G, $(date -Is)"
		run --providers 2>&1
	} > "$log"
	grep -q '^provider wine_libm:' "$log" || { rm -rf "$U.new"; fail "$G is not an x86 gexc (no wine_libm provider; log $log)"; }
	n=0 bad=0 nohints=0 nocode=0
	while read -r f b; do
		[ -z "$f" ] || [ "${f#\#}" != "$f" ] && continue
		case $f in /*) ;; *) f=$D/$f ;; esac
		n=$((n + 1))
		out=$(run "$f" --pe-base "$b" --jobs "${UNITS_JOBS:-12}" 2>&1); rc=$?
		echo "$out" | grep -a "indirect-call hints\|rror\|^\[cgen\] [0-9]\| at 0x" | cut -c1-300 >> "$log"
		# An image with no code (resources only: normaliz.dll, tzres.dll) has
		# nothing to link and no unit; not a failure.
		if [ $rc != 0 ] && echo "$out" | grep -q "wasm-ld: error: no input files"; then
			rc=0; nocode=$((nocode + 1)); echo "no code: $f at $b" >> "$log"
		fi
		[ $rc = 0 ] || { bad=$((bad + 1)); echo "FAILED ($rc): $f at $b" >> "$log"; }
		[ -d "$D/icall" ] && echo "$out" | grep -q "indirect-call hints: none" && nohints=$((nohints + 1))
	done < "$D/units.list"
	[ $bad = 0 ] || { rm -rf "$U.new"; fail "$bad of $n images did not build (log $log)"; }
	mv "$U.new" "$U" && touch "$U/.complete"
	echo "units: built $n images for inputs $K in $(( $(date +%s) - t0 )) s ($nohints without an indirect-call profile, $nocode with no code; log $log)" >&2
fi
# The i386 trampoline page's unit, which no image in units.list is: a 32-bit Wine
# page with units places it with its first image and wants it. Added to a set
# built before it was (cached: a no-op once there).
if ! GEX_CGEN_CACHE="$U" GEX_CGEN_ICALL_COUNT=0 systemd-run --user --scope -q -p TasksMax=4096 -p MemoryMax=32G nice -n 19 "$G" cgen-image --nt-trampolines32 > "$U.tramp.log" 2>&1; then
	fail "the trampoline page's unit did not build (log $U.tramp.log)"
fi
rm -f "$U.tramp.log"
touch "$U"
# Keep the three most recently used sets.
ls -1dt "$D"/ukeys/*/ 2>/dev/null | tail -n +4 | while read -r old; do
	old=${old%/}; [ "$old" = "$U" ] || rm -rf "$old" "$old.log"
done
echo "$U"
