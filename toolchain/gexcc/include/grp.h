/* musl's <grp.h>, with fgetgrent and putgrent where glibc has them: in its
 * default (and BSD) source, not only _GNU_SOURCE (accountsservice's mocklibc
 * calls fgetgrent with no feature macro). */
#include_next <grp.h>

#if defined(_BSD_SOURCE) && !defined(_GNU_SOURCE)
#ifdef __cplusplus
extern "C" {
#endif
struct group *fgetgrent(FILE *);
int putgrent(const struct group *, FILE *);
#ifdef __cplusplus
}
#endif
#endif
