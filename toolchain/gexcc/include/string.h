/* musl's <string.h>, with glibc's GNU basename under _GNU_SOURCE: the part of
 * the path after its last slash, the argument left unchanged and a path
 * ending in a slash giving "" (libsemanage, coreutils' gnulib users). musl
 * has only the POSIX basename of <libgen.h>; as in glibc, <libgen.h> wins
 * when both are included, in either order. */
#include_next <string.h>

#if defined(_GNU_SOURCE) && !defined(_LIBGEN_H) && !defined(basename)
#ifdef __cplusplus
extern "C" {
#endif
char *__gexcc_gnu_basename(const char *);
#ifdef __cplusplus
}
#endif
#define basename __gexcc_gnu_basename
#endif
