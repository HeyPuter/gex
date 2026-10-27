/* musl's <grp.h>, with fgetgrent and putgrent where glibc has them: in its
 * default (and BSD) source, not only _GNU_SOURCE (accountsservice's mocklibc
 * calls fgetgrent with no feature macro). FILE as musl names it, since
 * <grp.h> declares the type only under _GNU_SOURCE. */
#include_next <grp.h>

#if defined(_BSD_SOURCE) && !defined(_GNU_SOURCE)
#ifdef __cplusplus
extern "C" {
#endif
struct _IO_FILE;
struct group *fgetgrent(struct _IO_FILE *);
int putgrent(const struct group *, struct _IO_FILE *);
#ifdef __cplusplus
}
#endif
#endif
