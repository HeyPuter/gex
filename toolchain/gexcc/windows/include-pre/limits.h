/* limits.h for Windows programs (gexcc-win): msvcrt's, with MinGW's SSIZE_MAX */
#pragma once
#include_next <limits.h>
#ifndef SSIZE_MAX
#ifdef _WIN64
#define SSIZE_MAX _I64_MAX
#else
#define SSIZE_MAX INT_MAX
#endif
#endif
