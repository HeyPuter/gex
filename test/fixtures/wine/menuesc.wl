# the menuesc test, at 1024x768: open the Start menu (the taskbar's button at
# the bottom left) and press Escape; click menuesc.exe's window (at 300,200)
# to make it foreground again; then right-click the empty desktop and press
# Escape. In seconds from menuesc.exe's window (333x222) being up.
window 333 222
3     click 20 758
6     tap esc
9     click 450 300
12    move 800 100
12.2  button 273 1
12.3  button 273 0
15    tap esc
