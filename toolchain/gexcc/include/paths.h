/* musl's <paths.h>, with glibc's utmpx names for the files: the same files as
 * _PATH_UTMP and _PATH_WTMP, as in glibc (accountsservice watches
 * _PATH_WTMPX). */
#include_next <paths.h>

#ifndef _PATH_UTMPX
#define _PATH_UTMPX _PATH_UTMP
#endif
#ifndef _PATH_WTMPX
#define _PATH_WTMPX _PATH_WTMP
#endif
