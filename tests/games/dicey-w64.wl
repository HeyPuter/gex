# Dicey Dungeons (x64, W64), GEX_WL_INPUT: the first run's language screen
# (English and Start Windowed already ticked) and its PLAY button, into the
# title menu. Seconds from the 19th frame, when the language screen is
# drawn (about 130 s after the first frame on a loaded gooner). Positions
# are on Wine's toplevel surface, which has the window frame: the client
# area starts at (4, 30), so PLAY's centre (640, 602) is (644, 632). The
# press is held: Starling reads the mouse once per game frame.
frames 19
10  move 644 632
12  button 272 1
13  button 272 0
60  move 644 632
62  button 272 1
63  button 272 0
