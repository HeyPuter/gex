/* musl's <shadow.h>, with glibc's fgetspent_r and sgetspent_r (rt/shadow_r.c):
 * accountsservice reads /etc/shadow with fgetspent_r. */
#include_next <shadow.h>

#if defined(_BSD_SOURCE) || defined(_GNU_SOURCE)
#ifdef __cplusplus
extern "C" {
#endif
struct _IO_FILE;
int fgetspent_r(struct _IO_FILE *, struct spwd *, char *, size_t, struct spwd **);
int sgetspent_r(const char *, struct spwd *, char *, size_t, struct spwd **);
#ifdef __cplusplus
}
#endif
#endif
