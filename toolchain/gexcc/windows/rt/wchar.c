/* What Wine's <wchar.h> has inline but libc++ (built separately) calls. */
#include <stddef.h>
typedef unsigned short wc;
wc *wmemchr(const wc *s, wc c, size_t n) { for (; n; n--, s++) if (*s == c) return (wc *)s; return 0; }
int wmemcmp(const wc *a, const wc *b, size_t n) { for (; n; n--, a++, b++) if (*a != *b) return *a > *b ? 1 : -1; return 0; }
wc *wmemcpy(wc *d, const wc *s, size_t n) { return __builtin_memcpy(d, s, n * sizeof(wc)); }
wc *wmemmove(wc *d, const wc *s, size_t n) { return __builtin_memmove(d, s, n * sizeof(wc)); }
wc *wmemset(wc *d, wc c, size_t n) { for (size_t i = 0; i < n; i++) d[i] = c; return d; }
