/* musl's <stdio.h>, with glibc's renameat2 flags under _GNU_SOURCE (glibc
 * has had them there since 2.28; musl only in <linux/fs.h>, which clashes
 * with <sys/mount.h>). */
#include_next <stdio.h>

#if defined(_GNU_SOURCE) && !defined(RENAME_NOREPLACE)
#define RENAME_NOREPLACE (1 << 0)
#define RENAME_EXCHANGE (1 << 1)
#define RENAME_WHITEOUT (1 << 2)
#endif
