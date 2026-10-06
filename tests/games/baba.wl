# Baba Is You (Linux), GEX_WL_INPUT: through its credits screen into the
# first level, where Baba walks right, pushes the rocks and wins on the
# flag; the world map follows. Seconds from the 20th frame.
#
# The credits screen (IGF, Nordic Game Jam, MP2) waits for a click. A fresh
# save then loads its ~3,900 sprites and goes straight into the first level,
# with no title menu. Chowdren reads the mouse and keys as state, once per
# game frame, so a press and release in the same frame (`click` and `tap`
# are 0.1 s apart) is missed when frames are slow under translation: every
# press here is held for a second or more.
#
# The pointer goes over the window first: with no X window manager, X
# keyboard focus follows the pointer.
frames 20
30  move 400 240
40  button 272 1
42  button 272 0
300 key right 1
300.8 key right 0
340 key right 1
340.8 key right 0
380 key right 1
380.8 key right 0
420 key right 1
420.8 key right 0
460 key right 1
460.8 key right 0
500 key right 1
500.8 key right 0
540 key right 1
540.8 key right 0
580 key right 1
580.8 key right 0
620 key right 1
620.8 key right 0
660 key right 1
660.8 key right 0
