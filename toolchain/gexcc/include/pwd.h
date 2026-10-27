/* musl's <pwd.h>, with fgetpwent and putpwent where glibc has them: in its
 * default (and BSD) source, not only _GNU_SOURCE. */
#include_next <pwd.h>

#if defined(_BSD_SOURCE) && !defined(_GNU_SOURCE)
#ifdef __cplusplus
extern "C" {
#endif
struct passwd *fgetpwent(FILE *);
int putpwent(const struct passwd *, FILE *);
#ifdef __cplusplus
}
#endif
#endif
