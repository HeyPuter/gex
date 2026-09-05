#!/bin/bash
# Launch Minecraft 26.2 without PrismLauncher.
#
# Prism captures the game's stdout and stderr into its own console, so the
# driver's diagnostics never reach a file the harness can read -- which makes
# every rendering question about this game unanswerable through the launcher.
# Invoking the client's main class directly keeps stderr, and lets Quick Play
# put the game straight into a world rather than the title screen.
#
#   app-minecraft-direct.sh <out.png> [budget] [world]
#
# The classpath is Prism's, read from a file the caller prepared with
#   grep -aoE '\-cp [^ ]+ org\.prismlauncher\.EntryPoint' <prism log>
# because resolving it from the version JSON would duplicate the launcher.
OUT=${1:-/tmp/mcd.png}; BUDGET=${2:-300}; WORLD=${3:-}
# GEX_MC_SYSTEM=1 runs the system Mesa instead of this build -- the reference
# picture for this game. Asking run-native.sh for another driver does not work:
# it points LIBGL_DRIVERS_PATH at a megadriver holding only gex_wgpu, so the
# game fails its own GL 3.3 check and leaves an error dialog.
#
# The driver path has to be named explicitly even for the system Mesa. Left to
# find its own, this game reports
#
#   glx: failed to create drisw screen
#   Driver does not support OpenGL 3.3
#
# while glxinfo on the same display, under the same environment, reports 4.6 --
# so the failure looks like the platform refusing a software context and is not
# one. GLFW dlopens libGLX rather than linking it (see the note about toolkits
# needing libGLX.so.0), and the loader does not resolve the driver the same way
# inside that process.
S=$(cd -- "$(dirname "$0")" && pwd)
RUNNER="$S/run-native.sh"
[ "${GEX_MC_SYSTEM:-0}" = 1 ] && RUNNER="env LIBGL_ALWAYS_SOFTWARE=1 LIBGL_DRIVERS_PATH=${GEX_SYSTEM_DRI:-/usr/lib/dri} MESA_LOADER_DRIVER_OVERRIDE=llvmpipe"
# ...and must not name a gallium driver either. Setting GALLIUM_DRIVER=gex_wgpu
# for the reference run asks the system Mesa for a driver it does not have,
# which is the *other* way to get "glx: failed to create drisw screen".
GALLIUM="GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}"
[ "${GEX_MC_SYSTEM:-0}" = 1 ] && GALLIUM=""
INST=/home/velzie/.local/share/PrismLauncher/instances/26.2
JAVA=/home/velzie/.local/share/PrismLauncher/java/java-runtime-epsilon/bin/java
CP=$(cat "${GEX_MC_CP:-/tmp/mc-cp.txt}")
# No -Djava.library.path: PrismLauncher points it at an instance "natives"
# directory that is empty here, and setting it *replaces* the default search
# path rather than adding to it, which was enough to stop the system Mesa
# resolving its driver ("glx: failed to create drisw screen") while the same
# command without it works. LWJGL 3.4 extracts and loads its own natives by
# absolute path, so nothing needs it.
export GEX_DISPLAY=${GEX_DISPLAY:-:79}
export DISPLAY=$GEX_DISPLAY
bash "$S/../ensure-display.sh" >/dev/null || exit 2
python3 "$S/killmatch.py" net.minecraft.client.main.Main EntryPoint prismlauncher >/dev/null 2>&1
sleep 3
# An array, so a world whose folder name has spaces or brackets -- "New World
# (2)" is the default second save -- survives as one argument.
QUICK=()
[ -n "$WORLD" ] && QUICK=(--quickPlaySingleplayer "$WORLD")
( env $GALLIUM timeout $((BUDGET + 90)) \
  $RUNNER "$JAVA" -Duser.language=en -Xms512m -Xmx4096m -cp "$CP" \
    net.minecraft.client.main.Main \
    --username velzie_ --version 26.2 --gameDir "$INST/minecraft" \
    --assetsDir /home/velzie/.local/share/PrismLauncher/assets \
    --assetIndex 32 --uuid 00000000000000000000000000000000 \
    --accessToken 0 --userType legacy --versionType release \
    --width 854 --height 480 "${QUICK[@]}" >"$OUT.log" 2>&1 & )
prev=""; stable=0; t=0
while [ "$t" -lt "$BUDGET" ]; do
  sleep 15; t=$((t + 15))
  timeout 25 import -window root "$OUT" 2>/dev/null || { echo "t=${t}s stalled"; continue; }
  [ -s "$OUT" ] || continue
  k=$(timeout 25 magick "$OUT" -depth 4 -format '%k' info: 2>/dev/null)
  m=$(timeout 25 magick identify -format '%[fx:int(mean*255)]' "$OUT" 2>/dev/null)
  echo "t=${t}s mean=$m colours=$k"
  # Settle on the *mean*, not the colour count. The loading frame and the drawn
  # title screen differ by only 171 vs ~185 colours -- a threshold between them
  # is indistinguishable from noise, and one set at 200 never fires at all, so
  # the poll runs to its budget and whatever it last captured gets read as the
  # result. That is how a loading frame was mistaken for a rendering defect and
  # bisected to an innocent commit. The panorama is the whole background, so
  # its absence halves the brightness: mean 29 without it, ~40 with it.
  if [ "${m:-0}" -ge 35 ]; then
    stable=$((stable+1)); [ "$stable" -ge 2 ] && break
  else stable=0; fi
  prev=$m
done
python3 "$S/killmatch.py" net.minecraft.client.main.Main >/dev/null 2>&1
[ -s "$OUT" ] && magick identify -format 'final %wx%h mean=%[fx:int(mean*255)]\n' "$OUT"
echo "--- driver messages ---"
grep -aoE "gex-wgpu: .{0,160}" "$OUT.log" 2>/dev/null | sed 's/[0-9]\+/N/g' | sort | uniq -c | sort -rn | head -15
