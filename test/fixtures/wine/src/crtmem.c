/* The CRT's mem* and str* functions against reference C, on random data:
 * unaligned, overlapping, zero-length and page-crossing cases. Under gex the
 * CRT's own may be export providers (native wasm, docs/merge-plan.md §9.9),
 * so running this with GEX_PROVIDERS=0 and =1 compares the lifted CRT and
 * the providers with the same reference. Comparisons are checked by sign.
 * Calls go through volatile pointers so the compiler cannot expand them. */
#include <stdio.h>
#include <string.h>
#include <wchar.h>
#include <windows.h>

static void *(*volatile c_memcpy)(void *, const void *, size_t) = memcpy;
static void *(*volatile c_memmove)(void *, const void *, size_t) = memmove;
static void *(*volatile c_memset)(void *, int, size_t) = memset;
static int (*volatile c_memcmp)(const void *, const void *, size_t) = memcmp;
static void *(*volatile c_memchr)(const void *, int, size_t) = memchr;
static size_t (*volatile c_strlen)(const char *) = strlen;
static int (*volatile c_strcmp)(const char *, const char *) = strcmp;
static int (*volatile c_strncmp)(const char *, const char *, size_t) = strncmp;
static char *(*volatile c_strchr)(const char *, int) = strchr;
static char *(*volatile c_strcpy)(char *, const char *) = strcpy;
static char *(*volatile c_strcat)(char *, const char *) = strcat;
static size_t (*volatile c_wcslen)(const wchar_t *) = wcslen;
static int (*volatile c_wcscmp)(const wchar_t *, const wchar_t *) = wcscmp;

static unsigned seed = 12345;
static unsigned rnd(void) {
    seed = seed * 1103515245u + 12345u;
    return seed >> 8;
}
static int sign(int v) {
    return (v > 0) - (v < 0);
}

#define N 1024
static unsigned char a[N], b[N], ra[N], rb[N];
static int failures;
static void fail(const char *what, int i) {
    if (failures++ < 20) {
        printf("FAIL %s (case %d)\n", what, i);
    }
}
/* Random bytes; with `zeros`, about one in `zeros` is 0 (string ends). */
static void fill(unsigned char *p, int n, int zeros) {
    for (int i = 0; i < n; ++i) {
        p[i] = (unsigned char)(rnd() % 255 + 1);
        if (zeros && rnd() % zeros == 0) {
            p[i] = 0;
        }
    }
}

int main(void) {
    int cases = 4000, f0;
    /* memmove (overlapping, both directions), memset */
    f0 = failures;
    for (int i = 0; i < cases; ++i) {
        fill(a, N, 0);
        memcpy(ra, a, N);
        int n = rnd() % 300, d = rnd() % 64, s = rnd() % 64;
        if (rnd() % 8 == 0) n = 0;
        int op = rnd() % 2;
        if (op == 0) {
            void *r = c_memmove(a + d, a + s, n);
            unsigned char tmp[300];
            for (int k = 0; k < n; ++k) tmp[k] = ra[s + k];
            for (int k = 0; k < n; ++k) ra[d + k] = tmp[k];
            if (r != a + d || memcmp(a, ra, N)) fail("memmove", i);
        }
        if (op == 1) {
            int c = rnd() & 0x1ff;
            void *r = c_memset(a + d, c, n);
            for (int k = 0; k < n; ++k) ra[d + k] = (unsigned char)c;
            if (r != a + d || memcmp(a, ra, N)) fail("memset", i);
        }
    }
    for (int i = 0; i < cases; ++i) {
        fill(a, N, 0);
        memset(b, 0x5a, N);
        int n = rnd() % 300, d = rnd() % 64, s = rnd() % 64;
        void *r = c_memcpy(b + d, a + s, n);
        int bad = r != b + d;
        for (int k = 0; k < N; ++k) {
            unsigned char want = (k >= d && k < d + n) ? a[s + k - d] : 0x5a;
            bad |= b[k] != want;
        }
        if (bad) fail("memcpy", i);
    }
    printf("%s memcpy/memmove/memset\n", failures == f0 ? "ok  " : "FAIL");
    /* memcmp, memchr */
    f0 = failures;
    for (int i = 0; i < cases; ++i) {
        fill(a, N, 0);
        memcpy(b, a, N);
        int n = rnd() % 200, s = rnd() % 64, t = rnd() % 64;
        if (rnd() % 2) memcpy(b + t, a + s, n);
        if (n && rnd() % 2) b[t + rnd() % n] ^= (unsigned char)(1 + rnd() % 255);
        int want = 0;
        for (int k = 0; k < n; ++k) {
            if (a[s + k] != b[t + k]) {
                want = a[s + k] < b[t + k] ? -1 : 1;
                break;
            }
        }
        if (sign(c_memcmp(a + s, b + t, n)) != want) fail("memcmp", i);
        int c = a[s + (n ? rnd() % n : 0)] ^ (rnd() % 4 == 0);
        const unsigned char *wantp = 0;
        for (int k = 0; k < n; ++k) {
            if (a[s + k] == (unsigned char)c) {
                wantp = a + s + k;
                break;
            }
        }
        if (c_memchr(a + s, c, n) != wantp) fail("memchr", i);
    }
    printf("%s memcmp/memchr\n", failures == f0 ? "ok  " : "FAIL");
    /* strlen, strcmp, strncmp, strchr, strcpy, strcat */
    f0 = failures;
    for (int i = 0; i < cases; ++i) {
        fill(a, N - 1, 40);
        a[N - 1] = 0;
        memcpy(b, a, N);
        int s = rnd() % 64, t = rnd() % 64, n = rnd() % 80;
        if (rnd() % 2) memcpy(b + t, a + s, 100);
        if (rnd() % 3 == 0) b[t + rnd() % 60] ^= 1;
        size_t len = 0;
        while (a[s + len]) ++len;
        if (c_strlen((char *)a + s) != len) fail("strlen", i);
        int want = 0;
        for (int k = 0;; ++k) {
            if (a[s + k] != b[t + k]) { want = a[s + k] < b[t + k] ? -1 : 1; break; }
            if (!a[s + k]) break;
        }
        if (sign(c_strcmp((char *)a + s, (char *)b + t)) != want) fail("strcmp", i);
        want = 0;
        for (int k = 0; k < n; ++k) {
            if (a[s + k] != b[t + k]) { want = a[s + k] < b[t + k] ? -1 : 1; break; }
            if (!a[s + k]) break;
        }
        if (sign(c_strncmp((char *)a + s, (char *)b + t, n)) != want) fail("strncmp", i);
        int c = rnd() % 5 == 0 ? 0 : a[s + rnd() % (len + 1)];
        const unsigned char *wp = 0;
        for (int k = 0;; ++k) {
            if (a[s + k] == (unsigned char)c) { wp = a + s + k; break; }
            if (!a[s + k]) break;
        }
        if ((unsigned char *)c_strchr((char *)a + s, c) != wp) fail("strchr", i);
        memset(b, 0x77, N);
        char *r = c_strcpy((char *)b + t, (char *)a + s);
        if (r != (char *)b + t || memcmp(b + t, a + s, len + 1) || b[t + len + 1] != 0x77) fail("strcpy", i);
        size_t len2 = len;
        int s2 = rnd() % 64;
        size_t l2 = 0;
        while (a[s2 + l2] && l2 < 200) ++l2;
        if (len + l2 + t + 2 < N && a[s2 + l2] == 0) {
            r = c_strcat((char *)b + t, (char *)a + s2);
            if (r != (char *)b + t || memcmp(b + t + len2, a + s2, l2 + 1)) fail("strcat", i);
        }
    }
    printf("%s strlen/strcmp/strncmp/strchr/strcpy/strcat\n", failures == f0 ? "ok  " : "FAIL");
    /* wcslen, wcscmp */
    f0 = failures;
    static wchar_t wa[N / 2], wb[N / 2];
    for (int i = 0; i < cases; ++i) {
        for (int k = 0; k < N / 2; ++k) {
            wa[k] = (wchar_t)(rnd() % 30 == 0 ? 0 : 1 + rnd() % 65535);
        }
        wa[N / 2 - 1] = 0;
        memcpy(wb, wa, sizeof wa);
        int s = rnd() % 32, t = s;
        if (rnd() % 3 == 0) wb[t + rnd() % 40] ^= 1;
        size_t len = 0;
        while (wa[s + len]) ++len;
        if (c_wcslen(wa + s) != len) fail("wcslen", i);
        int want = 0;
        for (int k = 0;; ++k) {
            if (wa[s + k] != wb[t + k]) { want = wa[s + k] < wb[t + k] ? -1 : 1; break; }
            if (!wa[s + k]) break;
        }
        if (sign(c_wcscmp(wa + s, wb + t)) != want) fail("wcscmp", i);
    }
    printf("%s wcslen/wcscmp\n", failures == f0 ? "ok  " : "FAIL");
    /* Page-crossing: a buffer ending at a page boundary. */
    f0 = failures;
    unsigned char *page = VirtualAlloc(0, 8192, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    for (int i = 0; i < 200; ++i) {
        int n = 1 + rnd() % 200;
        unsigned char *p = page + 4096 - n / 2;
        fill(p, n - 1, 0);
        p[n - 1] = 0;
        if (c_strlen((char *)p) != (size_t)n - 1) fail("strlen across a page", i);
        c_memset(p, 0x33, n);
        for (int k = 0; k < n; ++k) if (p[k] != 0x33) { fail("memset across a page", i); break; }
    }
    printf("%s page-crossing\n", failures == f0 ? "ok  " : "FAIL");
    printf("%d failures\n", failures);
    return failures != 0;
}
