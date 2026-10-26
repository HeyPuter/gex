#!/bin/sh
# Rebuild the PE fixtures of tests/recover_i386.rs and tests/recover_pe64.rs
# (committed, so the tests need no cross compiler).
set -eu
cd "$(dirname "$0")"
i686-w64-mingw32-gcc -O2 -nostdlib -fno-asynchronous-unwind-tables -Wl,-e,_entry \
    -o switch32.exe switch32.c -lkernel32
i686-w64-mingw32-gcc -O2 -nostdlib -fno-asynchronous-unwind-tables -Wl,-e,_entry \
    -Wl,--dynamicbase -o vtext32.exe vtext32.c -lkernel32
i686-w64-mingw32-gcc -O2 -nostdlib -fno-asynchronous-unwind-tables -Wl,-e,_entry \
    -Wl,--dynamicbase -o ptext32.exe ptext32.c -lkernel32
x86_64-w64-mingw32-gcc -O2 -nostdlib -fno-asynchronous-unwind-tables -Wl,-e,entry \
    -Wl,--dynamicbase -o vtail64.exe vtail64.c -lkernel32
