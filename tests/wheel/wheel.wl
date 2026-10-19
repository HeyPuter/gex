# tests/wheel/wheel.sh: wheel.exe's scrolls, in seconds from its window being
# up (640x480, the screen): a fixed delay from the first frame was too short
# on a loaded machine.
window 640 480
2     move 320 240
3     scroll 100            # one mouse notch down
5     scroll 2.5 x 40       # a trackpad: 40 deltas of 2.5 px down, 1/60 s apart
8     hscroll 100           # one notch right
