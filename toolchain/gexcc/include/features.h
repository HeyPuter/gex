/* gexcc: features.h over musl's. glibc's _GNU_SOURCE turns on the LFS64
 * names (pread64, off64_t, stat64), and programs written on glibc call them
 * after defining _GNU_SOURCE alone; musl gives them only for an explicit
 * _LARGEFILE64_SOURCE, as plain aliases (its off_t is 64-bit already). */
#include_next <features.h>

#if defined(_GNU_SOURCE) && !defined(_LARGEFILE64_SOURCE)
#define _LARGEFILE64_SOURCE 1
#endif
