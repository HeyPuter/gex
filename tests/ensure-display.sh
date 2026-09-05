#!/bin/bash
# Bring up :77 if it is not there. The headless X server keeps getting killed
# along with whatever process group a run belonged to, and a run that starts
# without it fails in LWJGL with an X11 error that looks nothing like the GL
# bug being chased -- so every run checks first.
D=${GEX_DISPLAY:-:77}

# The window manager is checked separately from the display, because the two
# die separately. Killing an application's process group takes openbox with it
# and leaves Xvfb alone -- and this script used to `exit 0` the moment the
# display answered, so the manager was never restarted. Without one, a GLUT
# program that reads window properties dies with
#
#   X Error of failed request: BadWindow (invalid Window parameter)
#   Major opcode of failed request: 20 (X_GetProperty)
#
# which is not a GL error and looks nothing like one. It cost a whole app
# sweep: 193 of 337 programs "exited" and the run read as a driver collapse
# until the missing manager turned up. Asked through EWMH rather than by
# process name, because `pgrep -f openbox` also matches this script.
ensure_wm() {
  if DISPLAY=$D timeout 5 xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null \
       | grep -q "window id"; then
    return 0
  fi
  # A manager that is running but has not claimed EWMH is worse than none:
  # the check above says there is none, and the start below then fails with
  # "A window manager is already running on screen 0" -- so ensure_wm reports
  # FAILED forever and every gate that honours it refuses to run. Clear any
  # such process first. Safe when there is none, and this only ever runs on a
  # headless test display.
  if pgrep -x openbox >/dev/null 2>&1; then
    pkill -x openbox >/dev/null 2>&1
    sleep 1
  fi
  # Through DISPLAY, not --display: openbox rejects that argument outright
  # ("Invalid command line argument"), so this exited nonzero every time and
  # the manager was never actually started -- the failure this whole
  # function exists to prevent, hidden inside the fix for it.
  DISPLAY=$D setsid nohup openbox >/tmp/openbox${D#:}.log 2>&1 </dev/null &
  for _ in $(seq 20); do
    DISPLAY=$D timeout 3 xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null \
      | grep -q "window id" && return 0
    sleep 0.3
  done
  echo "window manager on $D FAILED"
  return 1
}

if DISPLAY=$D timeout 5 xdpyinfo >/dev/null 2>&1; then ensure_wm; exit $?; fi
# Kill any server still holding the number before clearing its files: an
# earlier version deleted the socket of a live Xvfb, which left a server
# nothing could connect to and a display that could never be restarted.
pkill -x -f "Xvfb $D .*" >/dev/null 2>&1
sleep 1
rm -f /tmp/.X11-unix/X${D#:} /tmp/.X${D#:}-lock 2>/dev/null
setsid nohup Xvfb "$D" -screen 0 1280x1024x24 >/tmp/xvfb${D#:}.log 2>&1 </dev/null &
for _ in $(seq 30); do
  DISPLAY=$D timeout 3 xdpyinfo >/dev/null 2>&1 && break
  sleep 0.3
done
# Without a window manager xdotool windowactivate --sync never returns, the
# game never takes keyboard focus, and GLUT programs that read window
# properties die with BadWindow. See ensure_wm above.
ensure_wm
DISPLAY=$D timeout 5 xdpyinfo >/dev/null 2>&1 && echo "display $D up" || echo "display $D FAILED"
