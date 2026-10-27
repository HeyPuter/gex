/* musl's <utmpx.h>, with <paths.h> as glibc's brings it in (its
 * <bits/utmpx.h>): code that names _PATH_WTMPX includes only <utmpx.h>
 * (accountsservice's wtmp helper). */
#include_next <utmpx.h>
#include <paths.h>
