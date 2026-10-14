#!/bin/bash
# Build the session's winlogon (Wine fork programs/winlogon) as an i386 PE
# with MinGW, until it is part of the W3 Wine build.  usage: build-winlogon.sh WINE_SRC OUT.exe
set -eu
${CC:-i686-w64-mingw32-gcc} -O2 -Wall -o "$2" "$1/programs/winlogon/main.c" -ladvapi32
