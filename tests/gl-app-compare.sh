#!/bin/bash
# Photograph each GL program twice -- once on llvmpipe, once on gex_wgpu -- and
# rank them by how much the two differ.
#
#   tests/gl-app-compare.sh [outdir] [list]
#
# The app sweep (tests/gl-app-sweep.sh) answers "did the driver refuse or
# crash", which is not the same question as "does it look right". A program
# that binds every state the driver accepts, draws every frame it was asked to
# and produces the wrong pixels is a RAN in that sweep and a silent defect
# here. This is the other half: llvmpipe renders the same program, and the two
# images are compared.
#
# Read the output as a ranked list to look at, not as a pass/fail gate.
# Anything animated diverges by construction -- both runs are photographed at a
# fixed delay after their window appears, which is the best that can be done
# without a frame-accurate harness, and a spinning gear does not care. What the
# ranking is for is the top of it: a program whose two images share no
# structure at all is a real defect, and one whose images differ only in phase
# is not. RMSE is reported as a fraction, 0 identical and 1 maximally
# different.
#
# Both runs need a window, so both need the X display; programs that render
# offscreen and print numbers are not measured here and are the app sweep's
# business.
set -u

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${1:-/tmp/glcompare}
LIST=${2:-/tmp/glapps.txt}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
DELAY=${GEX_COMPARE_DELAY:-5}
# GEX_COMPARE_SELF=1 photographs gex_wgpu a second time and reports how far
# the driver's own two images are apart -- the animation floor, below which a
# difference from llvmpipe says nothing.
SELF=${GEX_COMPARE_SELF:-}
# GEX_COMPARE_REF2=1 photographs softpipe as a *second reference* and reports
# how far the two references are from each other.
#
# The animation floor retires one class of false defect; this retires the
# other, which is a reference that is simply wrong. llvmpipe advertises
# MESA_PRIM_QUADS and splits an immediate-mode quad on the opposite diagonal
# from softpipe, radeonsi and this driver, so `quad` and `quad-degenerate`
# scored 0.244 while being correct -- and the two references disagree with
# each other by 0.251, more than the driver disagrees with either. Four of the
# nine deterministic entries at the top of the tail turned out to be this:
# `quad`, `quad-degenerate`, `tri-viewport` and `simplex-noise`. The three
# that survived it -- `tri-fbo-tex-mip`, `dlist-degenerate`, `dlist-edgeflag`
# -- are the ones where the references agree to within 0.03 and the driver
# does not, and the first of those was two real driver bugs.
#
# Read the columns as: rmse means nothing unless it is well above both self
# and refdiff. Off by default because it is another run per program, and
# softpipe is slow enough to need a longer window.
REF2=${GEX_COMPARE_REF2:-}
export DISPLAY=${GEX_DISPLAY:-:77}

[ -d "$INST/lib/dri" ] || { echo "no driver at $INST/lib/dri" >&2; exit 2; }
xdpyinfo >/dev/null 2>&1 || { echo "no X display at $DISPLAY" >&2; exit 2; }
# The display has to be empty before this starts.
#
# A program is photographed by "the largest window that is not the root",
# which handles a window a previous program left behind but not a *foreign*
# window that happens to be bigger. Running anything else on the same display
# silently photographs it instead: a concurrent run of nine real applications
# on :78 produced a `textures` row of 0.792 whose three images were
# MaterialXGraphEditor, aseprite and SuperTuxKart. Nothing about that row was
# about textures, and nothing said so -- it simply appeared at the top of the
# ranking as the largest deterministic divergence in the run.
stale=$(xwininfo -root -children 2>/dev/null | grep -cE '^ +0x' || true)
if [ "${stale:-0}" -gt 0 ]; then
    echo "display $DISPLAY already has $stale window(s); close them first -- " \
         "this harness photographs the largest window on the display and " \
         "would photograph those" >&2
    xwininfo -root -children 2>/dev/null | grep -E '^ +0x' >&2
    exit 2
fi
mkdir -p "$OUT"
: > "$OUT/results.tsv"
printf 'app\trmse\tself\trefdiff\tnote\n' >> "$OUT/results.tsv"

# A program is photographed by its own window. With no window manager the
# window's name is whatever the program set, which is usually its argv[0] and
# sometimes a title -- so the largest window that is not the root is taken
# instead, which is right for the single-window programs this list holds.
shoot() {
    local driver=$1 out=$2 app=$3
    if [ "$driver" = llvmpipe ] || [ "$driver" = softpipe ]; then
        ( LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=$driver \
          timeout -s KILL $((DELAY + 20)) "$app" >"$out.log" 2>&1 & )
    else
        ( LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
          LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
          GALLIUM_DRIVER=gex_wgpu \
          timeout -s KILL $((DELAY + 10)) "$app" >"$out.log" 2>&1 & )
    fi
    sleep "$DELAY"
    # The largest window, not the first. With no window manager the root's
    # children are in stacking order, and a program that makes a small helper
    # window first would be photographed through it -- as would any window a
    # previous program left behind. Area is the one property that identifies
    # the window being rendered into.
    local window
    window=$(xwininfo -root -children 2>/dev/null | awk '
        match($0, /^ *(0x[0-9a-f]+) /, id) {
            if (match($0, /([0-9]+)x([0-9]+)\+/, wh)) {
                area = wh[1] * wh[2]
                if (area > best) { best = area; pick = id[1] }
            }
        }
        END { if (pick != "") print pick }')
    [ -n "$window" ] || return 1
    import -window "$window" "$out" 2>/dev/null || return 1
    [ -s "$out" ]
}

quiet_kill() {
    python3 - "$1" <<'PY' >/dev/null 2>&1
import os, signal, sys
want = sys.argv[1]
me = os.getpid()
for entry in os.listdir('/proc'):
    if not entry.isdigit() or int(entry) == me:
        continue
    try:
        with open(f'/proc/{entry}/cmdline', 'rb') as handle:
            argv = handle.read().decode('utf-8', 'replace')
    except OSError:
        continue
    # argv[0] only: matching the whole command line matches this script's own
    # invocation, which is how an earlier version killed the run it was in.
    if argv.split('\0')[0].endswith('/' + want) or argv.split('\0')[0] == want:
        try:
            os.kill(int(entry), signal.SIGKILL)
        except OSError:
            pass
PY
}

while read -r app; do
    name=$(basename "$app")
    case "$name" in
        aseprite|cemu|dolphin-emu|dolphin-emu-nogui|mpv) continue ;;
        hyprland|Hyprland|weston-*|looking-glass-client) continue ;;
    esac
    quiet_kill "$name"; sleep 1
    note=; rmse=
    if ! shoot llvmpipe "$OUT/$name-ref.png" "$app"; then
        note="no reference window"
    fi
    quiet_kill "$name"; sleep 1
    if [ -z "$note" ] && ! shoot gex_wgpu "$OUT/$name-gex.png" "$app"; then
        note="no gex window"
    fi
    # Did the program actually load this driver?
    #
    # The third class of false defect, after the animation floor and the
    # second reference, and the one that produced the largest divergence in
    # the whole run. `wayfire` scored 0.576 with an animation floor of zero
    # and two references agreeing to 0.0015 -- the perfect signature of a real
    # deterministic bug -- while rendering a black screen because it never
    # loaded gex_wgpu at all. It needs a DRM render node, the reference run
    # gets one from the system Mesa, and the driver under test lives in a
    # build that has no such device; wlroots said "Failed to open DRM render
    # device [...] Trying SW rendering instead" and fell back to pixman. The
    # two runs were not comparing two drivers.
    #
    # The driver announces itself on every successful init, so the absence of
    # the string is conclusive. One program in 59 -- and it was top of the
    # ranking.
    if [ -z "$note" ] && [ -f "$OUT/$name-gex.png.log" ] &&
       ! grep -q 'gex-wgpu' "$OUT/$name-gex.png.log"; then
        note="driver never loaded"
    fi
    quiet_kill "$name"; sleep 1
    if [ -z "$note" ]; then
        rmse=$(compare -metric RMSE "$OUT/$name-ref.png" "$OUT/$name-gex.png" \
               null: 2>&1 | sed -n 's/.*(\([0-9.e-]*\)).*/\1/p')
        [ -n "$rmse" ] || note="sizes differ or compare failed"
    fi
    # The animation floor: the same driver photographed twice. Anything
    # animated diverges from itself by construction, and until this column
    # existed there was no way to tell a program whose two images differ
    # because the driver is wrong from one whose two images differ because a
    # gear turned. `clear-repeat` sat near the top of the tail at 0.39 and
    # varies from *itself* by 0.22; `tri-cull-both` sat just below it at 0.34
    # and varies from itself by nothing, which is what a real defect looks
    # like. Off by default because it is a third run per program.
    self=
    if [ -n "$SELF" ] && [ -z "$note" ]; then
        if shoot gex_wgpu "$OUT/$name-gex2.png" "$app"; then
            self=$(compare -metric RMSE "$OUT/$name-gex.png" \
                   "$OUT/$name-gex2.png" null: 2>&1 \
                   | sed -n 's/.*(\([0-9.e-]*\)).*/\1/p')
        fi
        quiet_kill "$name"; sleep 1
    fi
    # The reference floor: llvmpipe against softpipe. A program whose two
    # references disagree as much as the driver disagrees with either is not
    # evidence of anything about the driver.
    refdiff=
    if [ -n "$REF2" ] && [ -z "$note" ]; then
        if shoot softpipe "$OUT/$name-ref2.png" "$app"; then
            refdiff=$(compare -metric RMSE "$OUT/$name-ref.png" \
                      "$OUT/$name-ref2.png" null: 2>&1 \
                      | sed -n 's/.*(\([0-9.e-]*\)).*/\1/p')
        fi
        quiet_kill "$name"; sleep 1
    fi
    printf '%s\t%s\t%s\t%s\t%s\n' "$name" "${rmse:--}" "${self:--}" \
        "${refdiff:--}" "$note" >> "$OUT/results.tsv"
    printf '%-32s %-10s %-10s %-10s %s\n' "$name" "${rmse:--}" "${self:--}" \
        "${refdiff:--}" "$note"
done < "$LIST"

echo
echo "---- most divergent ----"
awk -F'\t' 'NR>1 && $2 != "-"' "$OUT/results.tsv" \
    | sort -t"$(printf '\t')" -k2 -gr | head -25 \
    | awk -F'\t' '{printf "  %-32s %-10s %s\n",$1,$2,$3}'
echo "full results: $OUT/results.tsv"
