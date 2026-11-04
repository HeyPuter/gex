/* What programs built for MinGW-w64 read from its _mingw.h (gexcc-win's
 * compatibility headers; the C runtime is Wine's). */
#pragma once
#define __MINGW64_VERSION_MAJOR 12
#define __MINGW64_VERSION_MINOR 0
#define __MINGW64_VERSION_BUGFIX 0
#define __MINGW32_MAJOR_VERSION 3
#define __MINGW32_MINOR_VERSION 11
#ifndef _CRTIMP
#define _CRTIMP
#endif
#ifndef __MINGW_EXTENSION
#define __MINGW_EXTENSION __extension__
#endif
