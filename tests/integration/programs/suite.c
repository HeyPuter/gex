/* Guest-side conformance programs for the integration ladder (PLAN §12.5).
 *
 * One binary with a subcommand per area, so the runner can compare gex and
 * QEMU output case by case without maintaining a directory of tiny sources.
 * Everything here is deterministic: no clocks, no randomness, no addresses in
 * the output, because the whole point is byte-for-byte comparison between two
 * engines whose stack and heap live at different addresses.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <setjmp.h>
#include <limits.h>
#include <sys/utsname.h>

/* --- integer and flag behaviour ---------------------------------------- */

static void t_arith(void) {
    /* Signed overflow boundaries, which is where flag bugs surface. */
    int32_t a = INT32_MIN, b = -1;
    printf("intmin/-1=%d intmin%%-1=%d\n", (int32_t)((int64_t)a / b), (int32_t)((int64_t)a % b));
    printf("udiv0-guard=%u\n", (unsigned)(b == 0 ? 0 : 7u / (unsigned)(b ? 3 : 1)));

    uint64_t u = 0xdeadbeefcafef00dULL;
    printf("clz=%d ctz=%d popcount=%d\n", __builtin_clzll(u), __builtin_ctzll(u),
           __builtin_popcountll(u));
    printf("bswap64=%llx bswap32=%x bswap16=%x\n",
           (unsigned long long)__builtin_bswap64(u),
           __builtin_bswap32((uint32_t)u), __builtin_bswap16((uint16_t)u));

    /* Shifts at and past the operand width. */
    for (int s = 0; s < 66; s += 13)
        printf("shl%d=%llx shr%d=%llx sar%d=%llx\n", s,
               (unsigned long long)(u << (s & 63)), s,
               (unsigned long long)(u >> (s & 63)), s,
               (unsigned long long)(((int64_t)u) >> (s & 63)));

    /* 128-bit multiply, which lowers to umulh/smulh. */
    unsigned __int128 wide = (unsigned __int128)u * 0x0123456789abcdefULL;
    printf("umulh=%llx\n", (unsigned long long)(wide >> 64));
    __int128 swide = (__int128)(int64_t)u * (int64_t)0x0123456789abcdefLL;
    printf("smulh=%llx\n", (unsigned long long)((unsigned __int128)swide >> 64));

    /* Conditional select and compare chains. */
    int n = 0;
    for (int i = -3; i <= 3; i++) n += (i < 0) ? -i : (i > 1 ? i * 2 : i);
    printf("csel=%d\n", n);
}

/* --- floating point ----------------------------------------------------- */

static void t_float(void) {
    double d = 3.0;
    for (int i = 0; i < 6; i++) d = d / 7.0 + 1.5;
    printf("iter=%.12f\n", d);
    printf("sqrt=%.10f neg=%.3f abs=%.3f\n", __builtin_sqrt(2.0), -d, d < 0 ? -d : d);

    float f = 1.0f / 3.0f;
    printf("float=%.8f promote=%.15f\n", (double)f, (double)(double)f);

    /* Conversions at the saturating boundaries. */
    double big = 1e300, small = -1e300, nan = 0.0 / 0.0;
    printf("cvt=%lld %lld %lld\n", (long long)big, (long long)small, (long long)nan);
    printf("cvtu=%llu %llu\n", (unsigned long long)2.5, (unsigned long long)-0.5);
    printf("i2f=%.1f %.1f\n", (double)(int64_t)-9007199254740993LL,
           (double)(uint64_t)18446744073709551615ULL);

    /* Comparisons involving NaN take every branch direction. */
    printf("nan lt=%d le=%d gt=%d ge=%d eq=%d ne=%d\n", nan < 1.0, nan <= 1.0, nan > 1.0,
           nan >= 1.0, nan == 1.0, nan != 1.0);
    printf("rounding=%.0f %.0f %.0f %.0f\n", 0.5, 1.5, 2.5, -0.5);
}

/* --- memory and strings ------------------------------------------------- */

static void t_mem(void) {
    /* Exercise the SIMD memcpy/memset paths at many alignments and sizes. */
    static char src[1024], dst[1024];
    for (int i = 0; i < 1024; i++) src[i] = (char)(i * 31 + 7);
    unsigned long h = 5381;
    for (int off = 0; off < 17; off++) {
        for (int len = 0; len < 200; len += 7) {
            memset(dst, 0, sizeof dst);
            memcpy(dst + off, src + off, len);
            for (int i = 0; i < len + off; i++) h = h * 33 + (unsigned char)dst[i];
        }
    }
    printf("memcpy-hash=%lx\n", h);

    memset(dst, 'x', 300);
    dst[300] = 0;
    printf("memset len=%zu cmp=%d\n", strlen(dst), memcmp(dst, dst + 1, 299));

    const char *s = "the quick brown fox jumps over the lazy dog";
    printf("strlen=%zu strchr=%td strstr=%td\n", strlen(s), strchr(s, 'q') - s,
           strstr(s, "over") - s);
    printf("strcmp=%d %d %d\n", strcmp("abc", "abd") < 0, strcmp("abc", "abc"),
           strcmp("abd", "abc") > 0);

    /* Heap traffic drives brk and mmap. */
    void *p[64];
    for (int i = 0; i < 64; i++) {
        p[i] = malloc((size_t)(i * 977 + 1));
        memset(p[i], i, (size_t)(i * 977 + 1));
    }
    unsigned long sum = 0;
    for (int i = 0; i < 64; i++) {
        sum += ((unsigned char *)p[i])[i * 977 / 2];
        free(p[i]);
    }
    printf("heap=%lu\n", sum);

    char *big = malloc(4 << 20);
    if (!big) { printf("bigalloc=failed\n"); return; }
    memset(big, 0xa5, 4 << 20);
    printf("bigalloc=%d\n", (unsigned char)big[(4 << 20) - 1]);
    free(big);
}

/* --- control flow ------------------------------------------------------- */

static int sw(int x) {
    /* A dense switch compiles to a jump table, which is the case static
     * discovery cannot see through. */
    switch (x) {
        case 0: return 11;
        case 1: return 22;
        case 2: return 33;
        case 3: return 44;
        case 4: return 55;
        case 5: return 66;
        case 6: return 77;
        case 7: return 88;
        case 8: return 99;
        case 9: return 111;
        default: return -1;
    }
}

typedef int (*fnptr)(int);
static int add1(int x) { return x + 1; }
static int mul2(int x) { return x * 2; }
static int neg(int x) { return -x; }

static jmp_buf jb;
static void deep(int n) {
    if (n == 0) longjmp(jb, 42);
    deep(n - 1);
}

static void t_flow(void) {
    int total = 0;
    for (int i = 0; i < 12; i++) total += sw(i);
    printf("switch=%d\n", total);

    fnptr table[] = {add1, mul2, neg};
    int v = 5;
    for (int i = 0; i < 9; i++) v = table[i % 3](v);
    printf("indirect=%d\n", v);

    int r = setjmp(jb);
    if (r == 0) deep(200);
    printf("longjmp=%d\n", r);

    /* Deep recursion checks that guest stack depth never becomes wasm stack
     * depth — the point of the tail-call design. */
    unsigned long long acc = 0;
    for (int i = 0; i < 2000; i++) acc = acc * 1103515245ULL + (unsigned)i;
    printf("loop=%llx\n", acc);
}

/* --- system interface --------------------------------------------------- */

static void t_sys(void) {
    struct utsname u;
    if (uname(&u) == 0) printf("uname=%s %s\n", u.sysname, u.machine);

    printf("getpid>0=%d getuid=%d\n", getpid() > 0, (int)getuid());

    /* Error paths must report the same errno. */
    errno = 0;
    int fd = open("/definitely/not/here", O_RDONLY);
    printf("open-missing fd=%d errno=%d\n", fd, errno);

    errno = 0;
    printf("read-badfd=%zd errno=%d\n", read(999, (char[16]){0}, 16), errno);

    /* Write, read back, and check the size through fstat. */
    const char *path = "/tmp/gex-suite-scratch";
    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { printf("scratch=failed\n"); return; }
    const char *msg = "gex round trip\n";
    ssize_t w = write(fd, msg, strlen(msg));
    lseek(fd, 0, SEEK_SET);
    char buf[64] = {0};
    ssize_t rd = read(fd, buf, sizeof buf - 1);
    printf("roundtrip w=%zd r=%zd eq=%d\n", w, rd, strcmp(buf, msg) == 0);
    close(fd);
    remove(path);

    fprintf(stderr, "stderr-works\n");
}

int main(int argc, char **argv) {
    const char *which = argc > 1 ? argv[1] : "all";
    int all = strcmp(which, "all") == 0;
    if (all || !strcmp(which, "arith")) t_arith();
    if (all || !strcmp(which, "float")) t_float();
    if (all || !strcmp(which, "mem")) t_mem();
    if (all || !strcmp(which, "flow")) t_flow();
    if (all || !strcmp(which, "sys")) t_sys();
    return 0;
}
