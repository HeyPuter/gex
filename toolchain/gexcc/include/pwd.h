/* musl's <pwd.h>, with fgetpwent and putpwent where glibc has them: in its
 * default (and BSD) source, not only _GNU_SOURCE. */
#include_next <pwd.h>

#if defined(_BSD_SOURCE) && !defined(_GNU_SOURCE)
#ifdef __cplusplus
extern "C" {
#endif
struct _IO_FILE;
struct passwd *fgetpwent(struct _IO_FILE *);
int putpwent(const struct passwd *, struct _IO_FILE *);
#ifdef __cplusplus
}
#endif
#endif
