#!/bin/sh
# Rebuild the fixture of tests/relayout_x86.rs (committed, so the test needs
# no cross compiler): x86-64, static, with DWARF for the layout oracle.
set -eu
cd "$(dirname "$0")"
clang --target=x86_64-linux-gnu -O1 -g -fno-pic -fno-stack-protector -ffreestanding \
    -nostdlib -static -fuse-ld=lld -Wl,-e,entry -Wl,--image-base=0x10000000 -o relayout.elf relayout.c
