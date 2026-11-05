# the wheel tests's slow leg (wheel.exe SECONDS slow): wheel.wl's scrolls to an
# application whose WH_MOUSE hook sleeps 60 ms on each wheel message, so the
# trackpad's deltas arrive while one is handed over and not yet accepted; then
# a burst of 20 moves, all at once, while the hook sleeps on the first.
window 640 480
2     move 320 240
3     scroll 100            # one mouse notch down
5     scroll 2.5 x 40       # a trackpad: 40 deltas of 2.5 px down, 1/60 s apart
8     hscroll 100           # one notch right
10    move 410 240
10    move 420 240
10    move 430 240
10    move 440 240
10    move 450 240
10    move 460 240
10    move 470 240
10    move 480 240
10    move 490 240
10    move 500 240
10    move 510 240
10    move 520 240
10    move 530 240
10    move 540 240
10    move 550 240
10    move 560 240
10    move 570 240
10    move 580 240
10    move 590 240
