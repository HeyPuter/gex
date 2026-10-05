/* ELF providers' conformance: glibc's mem* and str* (docs/elf-providers.md
 * §4). Every size 0..256 plus large ones, every alignment pair 0..15,
 * overlapping moves, a NUL at every position, the sign conventions (only
 * the sign of memcmp: glibc's own variants disagree on its magnitude, e.g.
 * 1082130432 from __memcmp_sse2 and 129 from __memcmp_avx2_movbe for the
 * same two bytes, so no program can depend on it). Prints
 * results and checksums; run with providers and with GEX_PROVIDERS=0, the
 * outputs must match byte for byte. Built with -fno-builtin, so every call
 * goes through the PLT to libc. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint8_t src[4096 + 64], dst[4096 + 64], ref[4096 + 64];
static uint64_t h = 1469598103934665603ull;
static void mix(uint64_t v) { h = (h ^ v) * 1099511628211ull; }
static void mixbuf(const uint8_t *p, size_t n) { for (size_t i = 0; i < n; ++i) mix(p[i]); }
static void fill(uint8_t *p, size_t n, unsigned seed) { for (size_t i = 0; i < n; ++i) p[i] = (uint8_t)(i * 131u + seed * 7u + 1u); }

static int sgn(int x) { return (x > 0) - (x < 0); }

int main(void) {
    static const size_t sizes[] = {0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 1000, 4000};
    const int nsizes = sizeof sizes / sizeof sizes[0];
    /* memcpy, mempcpy, memset: every alignment pair. */
    for (int k = 0; k < nsizes; ++k) {
        size_t n = sizes[k];
        for (int sa = 0; sa < 16; ++sa)
            for (int da = 0; da < 16; ++da) {
                fill(src, sizeof src, (unsigned)(sa + da));
                memset(dst, 0xcc, sizeof dst);
                void *r = memcpy(dst + da, src + sa, n);
                mix(r == dst + da);
                void *e = mempcpy(dst + da, src + sa, n);
                mix(e == dst + da + n);
                mixbuf(dst, n + 32);
                r = memset(dst + da, sa * 17, n);
                mix(r == dst + da);
                mixbuf(dst, n + 32);
            }
    }
    printf("copy/fill %016llx\n", (unsigned long long)h);
    h = 1469598103934665603ull;
    /* memmove: overlapping both ways. */
    for (int k = 0; k < nsizes; ++k) {
        size_t n = sizes[k];
        for (int off = -17; off <= 17; ++off) {
            fill(dst, sizeof dst, (unsigned)(off + 40));
            memcpy(ref, dst, sizeof ref);
            void *r = memmove(dst + 32 + off, dst + 32, n);
            mix(r == dst + 32 + off);
            /* reference */
            for (size_t i = 0; i < n; ++i) src[i] = ref[32 + i];
            for (size_t i = 0; i < n; ++i) ref[32 + off + i] = src[i];
            mix(memcmp(dst, ref, n + 64) == 0);
        }
    }
    printf("move %016llx\n", (unsigned long long)h);
    h = 1469598103934665603ull;
    /* memcmp/bcmp: a difference at every position, both signs. */
    for (int k = 0; k < nsizes; ++k) {
        size_t n = sizes[k];
        for (int a = 0; a < 8; ++a) {
            fill(src, sizeof src, 3);
            memcpy(dst + a, src, n);
            mix((unsigned)sgn(memcmp(dst + a, src, n)));
            for (size_t i = 0; i < n; i += (n > 64 ? 7 : 1)) {
                uint8_t keep = dst[a + i];
                dst[a + i] = (uint8_t)(keep + 0x81);
                int c = memcmp(dst + a, src, n);
                mix((unsigned)sgn(c));
                mix((unsigned)sgn(memcmp(src, dst + a, n)));
                mix(bcmp(dst + a, src, n) != 0);
                dst[a + i] = keep;
            }
        }
    }
    printf("cmp %016llx\n", (unsigned long long)h);
    h = 1469598103934665603ull;
    /* strlen, strchr, memchr: a NUL / the byte at every position, every alignment. */
    for (int a = 0; a < 16; ++a)
        for (int len = 0; len < 300; ++len) {
            memset(dst, 'x', sizeof dst);
            dst[a + len] = 0;
            mix(strlen((char *)dst + a));
            dst[a + len / 2] = 'q';
            char *p = strchr((char *)dst + a, 'q');
            mix(p ? (uint64_t)(p - (char *)dst) : 99999);
            p = strchr((char *)dst + a, 0);
            mix(p ? (uint64_t)(p - (char *)dst) : 99999);
            p = strchr((char *)dst + a, 'z');
            mix(p ? (uint64_t)(p - (char *)dst) : 99999);
            void *m = memchr(dst + a, 'q', (size_t)len);
            mix(m ? (uint64_t)((uint8_t *)m - dst) : 99999);
            m = memchr(dst + a, 0, (size_t)len + 1);
            mix(m ? (uint64_t)((uint8_t *)m - dst) : 99999);
        }
    printf("str %016llx\n", (unsigned long long)h);
    h = 1469598103934665603ull;
    /* strcmp: equal, prefix, a difference at every position. */
    {
        char x[300], y[300];
        for (int len = 0; len < 260; ++len) {
            memset(x, 'a', sizeof x);
            x[len] = 0;
            memcpy(y, x, sizeof x);
            mix((unsigned)strcmp(x, y));
            for (int i = 0; i < len; ++i) {
                y[i] = (char)0xf0;
                mix((unsigned)strcmp(x, y));
                mix((unsigned)strcmp(y, x));
                y[i] = 'a';
            }
            if (len > 0) {
                y[len - 1] = 0;
                mix((unsigned)strcmp(x, y));
                mix((unsigned)strcmp(y, x));
            }
        }
    }
    printf("strcmp %016llx\n", (unsigned long long)h);
    h = 1469598103934665603ull;
    /* Through the library's own callers: strdup (malloc + memcpy), snprintf. */
    {
        char buf[512];
        for (int i = 0; i < 200; ++i) {
            int n = snprintf(buf, sizeof buf, "%d %s %.*s", i, "abcdefghijklmnopqrstuvwxyz", i % 27, "0123456789abcdefghijklmnopq");
            char *d = strdup(buf);
            mix((unsigned)n);
            mixbuf((uint8_t *)d, strlen(d));
            free(d);
        }
    }
    printf("libc %016llx\n", (unsigned long long)h);
    h = 1469598103934665603ull;
    return 0;
}
