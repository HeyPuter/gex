/* gexcc: features.h over musl's. glibc's _GNU_SOURCE turns on the LFS64
 * names (pread64, off64_t, stat64), and programs written on glibc call them
 * after defining _GNU_SOURCE alone; musl gives them only for an explicit
 * _LARGEFILE64_SOURCE, as plain aliases (its off_t is 64-bit already). */
#include_next <features.h>

#if defined(_GNU_SOURCE) && !defined(_LARGEFILE64_SOURCE)
#define _LARGEFILE64_SOURCE 1
#endif

/* glibc's internal name for ino_t (<bits/types.h>, which every glibc
 * header brings in), which programs written on glibc use as if it were
 * public (libselinux's matchpathcon.c). musl's ino_t is 64-bit everywhere. */
#ifndef __GEXCC_INO_T
#define __GEXCC_INO_T
typedef unsigned long long __ino_t;
typedef unsigned long long __ino64_t;
#endif
