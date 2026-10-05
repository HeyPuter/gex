/* The C library the Lua provider links against, beside musl's string, math,
 * strtod and printf (build.sh compiles those from emscripten's musl) and
 * emscripten's dlmalloc:
 *
 * - the heap: dlmalloc's sbrk, over guest pages the host hands out
 *   (gex.provider_alloc), in chunks that need not be contiguous;
 * - stdio: musl's, over three FILEs of its own: stdout and stderr write to
 *   the host's stderr (gex.provider_log), stdin is at its end, and no file
 *   ever opens, so io.open and loadfile fail the way a missing file does. Noita loads its scripts through its own
 *   file system (luaL_loadbuffer), never through Lua's;
 * - time, through gex.provider_time;
 * - what musl's stdio internals ask for and never need here (locks).
 */
#include <errno.h>
#include <locale.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "stdio_impl.h"

__attribute__((import_module("gex"), import_name("provider_alloc"))) int gex_provider_alloc(int size);
__attribute__((import_module("gex"), import_name("provider_log"))) void gex_provider_log(const char *p, int n);
__attribute__((import_module("gex"), import_name("provider_time"))) double gex_provider_time(int which);

/* ---- the heap ------------------------------------------------------------ */

#define CHUNK (64u << 20)
static uintptr_t brk_cur, brk_end;
void *sbrk(intptr_t increment) {
    if (increment < 0) {
        errno = ENOMEM;
        return (void *)-1;
    }
    if (brk_cur == 0 || brk_cur + (uintptr_t)increment > brk_end) {
        if (increment == 0 && brk_cur) {
            return (void *)brk_cur;
        }
        size_t n = (size_t)increment > CHUNK ? ((size_t)increment + 0xffffu) & ~0xffffu : CHUNK;
        int base = gex_provider_alloc((int)n);
        if (!base) {
            errno = ENOMEM;
            return (void *)-1;
        }
        brk_cur = (uintptr_t)(uint32_t)base;
        brk_end = brk_cur + n;
    }
    void *p = (void *)brk_cur;
    brk_cur += (uintptr_t)increment;
    return p;
}

/* ---- errno, abort, exit -------------------------------------------------- */

static int errno_value;
int *__errno_location(void) { return &errno_value; }
int *___errno_location(void) { return &errno_value; }
_Noreturn void abort(void) {
    static const char m[] = "lua51 provider: abort\n";
    gex_provider_log(m, sizeof m - 1);
    __builtin_trap();
}
_Noreturn void exit(int code) {
    (void)code;
    static const char m[] = "lua51 provider: exit() (os.exit) is not supported in the provider\n";
    gex_provider_log(m, sizeof m - 1);
    __builtin_trap();
}
char *getenv(const char *name) {
    (void)name;
    return NULL;
}
int system(const char *cmd) {
    (void)cmd;
    return -1;
}
int remove(const char *p) {
    (void)p;
    errno = ENOENT;
    return -1;
}
int rename(const char *a, const char *b) {
    (void)a, (void)b;
    errno = ENOENT;
    return -1;
}
char *tmpnam(char *s) {
    (void)s;
    return NULL;
}

/* ---- rand (C's, as LuaJIT's math.random uses its own; good enough) ------ */

static uint64_t seed = 1;
void srand(unsigned s) { seed = s - 1; }
int rand(void) {
    seed = 6364136223846793005ULL * seed + 1;
    return (int)(seed >> 33);
}

/* ---- ctype: the C locale --------------------------------------------- */

int isalnum(int c) { return (unsigned)((c | 32) - 'a') < 26 || (unsigned)(c - '0') < 10; }
int isalpha(int c) { return (unsigned)((c | 32) - 'a') < 26; }
int iscntrl(int c) { return (unsigned)c < 0x20 || c == 0x7f; }
int isdigit(int c) { return (unsigned)(c - '0') < 10; }
int isgraph(int c) { return (unsigned)(c - 0x21) < 0x5e; }
int islower(int c) { return (unsigned)(c - 'a') < 26; }
int isprint(int c) { return (unsigned)(c - 0x20) < 0x5f; }
int ispunct(int c) { return isgraph(c) && !isalnum(c); }
int isspace(int c) { return c == ' ' || (unsigned)(c - '\t') < 5; }
int isupper(int c) { return (unsigned)(c - 'A') < 26; }
int isxdigit(int c) { return isdigit(c) || (unsigned)((c | 32) - 'a') < 6; }
int tolower(int c) { return isupper(c) ? c | 32 : c; }
int toupper(int c) { return islower(c) ? c & 0x5f : c; }

static struct lconv lc = { .decimal_point = "." };
struct lconv *localeconv(void) { return &lc; }
char *setlocale(int cat, const char *loc) {
    (void)cat;
    return (!loc || !*loc || !strcmp(loc, "C")) ? "C" : NULL;
}

char *strerror(int e) {
    switch (e) {
    case ENOENT:
        return "No such file or directory";
    case ENOMEM:
        return "Out of memory";
    default:
        return "Error";
    }
}

/* ---- time ---------------------------------------------------------------- */

time_t time(time_t *t) {
    time_t now = (time_t)gex_provider_time(0);
    if (t) {
        *t = now;
    }
    return now;
}
clock_t clock(void) { return (clock_t)(gex_provider_time(1) * CLOCKS_PER_SEC); }
double difftime(time_t a, time_t b) { return (double)a - (double)b; }
/* UTC everywhere: the guest's time zone is not the provider's to know. */
static struct tm tmbuf;
struct tm *gmtime(const time_t *t) {
    long long s = *t, days = s / 86400, rem = s % 86400;
    if (rem < 0) {
        rem += 86400;
        --days;
    }
    tmbuf.tm_hour = (int)(rem / 3600);
    tmbuf.tm_min = (int)(rem % 3600 / 60);
    tmbuf.tm_sec = (int)(rem % 60);
    tmbuf.tm_wday = (int)((4 + days) % 7 + 7) % 7;
    /* Civil from days (Howard Hinnant's algorithm). */
    long long z = days + 719468, era = (z >= 0 ? z : z - 146096) / 146097;
    unsigned doe = (unsigned)(z - era * 146097);
    unsigned yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    long long y = (long long)yoe + era * 400;
    unsigned doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    unsigned mp = (5 * doy + 2) / 153;
    unsigned d = doy - (153 * mp + 2) / 5 + 1;
    unsigned m = mp < 10 ? mp + 3 : mp - 9;
    y += m <= 2;
    tmbuf.tm_year = (int)(y - 1900);
    tmbuf.tm_mon = (int)m - 1;
    tmbuf.tm_mday = (int)d;
    int leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    static const int before[12] = { 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334 };
    tmbuf.tm_yday = before[m - 1] + (int)d - 1 + (m > 2 && leap);
    tmbuf.tm_isdst = 0;
    return &tmbuf;
}
struct tm *localtime(const time_t *t) { return gmtime(t); }
time_t mktime(struct tm *tm) {
    long long y = tm->tm_year + 1900LL, m = tm->tm_mon + 1;
    y -= m <= 2;
    long long era = (y >= 0 ? y : y - 399) / 400;
    unsigned yoe = (unsigned)(y - era * 400);
    unsigned doy = (unsigned)((153 * (m > 2 ? m - 3 : m + 9) + 2) / 5 + tm->tm_mday - 1);
    unsigned doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    long long days = era * 146097 + (long long)doe - 719468;
    return (time_t)(days * 86400 + tm->tm_hour * 3600LL + tm->tm_min * 60LL + tm->tm_sec);
}
size_t strftime(char *s, size_t max, const char *fmt, const struct tm *tm) {
    /* The numeric conversions os.date uses; names in English. */
    static const char *const wd[] = { "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat" };
    static const char *const mo[] = { "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec" };
    size_t n = 0;
    char tmp[64];
    for (; *fmt; ++fmt) {
        const char *add = tmp;
        if (*fmt != '%') {
            tmp[0] = *fmt;
            tmp[1] = 0;
        } else {
            switch (*++fmt) {
            case 'Y': snprintf(tmp, sizeof tmp, "%d", tm->tm_year + 1900); break;
            case 'y': snprintf(tmp, sizeof tmp, "%02d", (tm->tm_year + 1900) % 100); break;
            case 'm': snprintf(tmp, sizeof tmp, "%02d", tm->tm_mon + 1); break;
            case 'd': snprintf(tmp, sizeof tmp, "%02d", tm->tm_mday); break;
            case 'H': snprintf(tmp, sizeof tmp, "%02d", tm->tm_hour); break;
            case 'M': snprintf(tmp, sizeof tmp, "%02d", tm->tm_min); break;
            case 'S': snprintf(tmp, sizeof tmp, "%02d", tm->tm_sec); break;
            case 'j': snprintf(tmp, sizeof tmp, "%03d", tm->tm_yday + 1); break;
            case 'a': add = wd[tm->tm_wday % 7]; break;
            case 'b': add = mo[tm->tm_mon % 12]; break;
            case 'p': add = tm->tm_hour < 12 ? "AM" : "PM"; break;
            case 'c':
                snprintf(tmp, sizeof tmp, "%s %s %2d %02d:%02d:%02d %d", wd[tm->tm_wday % 7], mo[tm->tm_mon % 12], tm->tm_mday,
                         tm->tm_hour, tm->tm_min, tm->tm_sec, tm->tm_year + 1900);
                break;
            case 'x': snprintf(tmp, sizeof tmp, "%02d/%02d/%02d", tm->tm_mon + 1, tm->tm_mday, (tm->tm_year + 1900) % 100); break;
            case 'X': snprintf(tmp, sizeof tmp, "%02d:%02d:%02d", tm->tm_hour, tm->tm_min, tm->tm_sec); break;
            case '%': add = "%"; break;
            default: tmp[0] = 0; break;
            }
        }
        size_t k = strlen(add);
        if (n + k >= max) {
            return 0;
        }
        memcpy(s + n, add, k);
        n += k;
    }
    s[n] = 0;
    return n;
}

/* ---- stdio --------------------------------------------------------------- */

/* musl's stdio over three FILEs of our own: stdout and stderr write to the
 * host's stderr (unbuffered), stdin is always at its end. No other file
 * ever opens. */
static size_t log_write(FILE *f, const unsigned char *buf, size_t len) {
    size_t pending = (size_t)(f->wpos - f->wbase);
    if (pending) {
        gex_provider_log((const char *)f->wbase, (int)pending);
    }
    if (len) {
        gex_provider_log((const char *)buf, (int)len);
    }
    f->wend = f->buf + f->buf_size;
    f->wpos = f->wbase = f->buf;
    return len;
}
static size_t no_read(FILE *f, const unsigned char *buf, size_t len) {
    (void)buf, (void)len;
    f->flags |= F_EOF;
    return 0;
}
static off_t no_seek(FILE *f, off_t off, int whence) {
    (void)f, (void)off, (void)whence;
    errno = ESPIPE;
    return -1;
}
static int no_close(FILE *f) {
    (void)f;
    return 0;
}
static unsigned char inbuf[BUFSIZ + UNGET];
hidden FILE __stdin_FILE = {
    .buf = inbuf + UNGET, .buf_size = sizeof inbuf - UNGET, .fd = 0, .flags = F_PERM | F_NOWR,
    .read = (size_t (*)(FILE *, unsigned char *, size_t))no_read, .seek = no_seek, .close = no_close, .lock = -1,
};
static unsigned char outbuf[UNGET];
hidden FILE __stdout_FILE = {
    .buf = outbuf + UNGET, .buf_size = 0, .fd = 1, .flags = F_PERM | F_NORD, .lbf = -1,
    .write = log_write, .seek = no_seek, .close = no_close, .lock = -1,
};
static unsigned char errbuf[UNGET];
hidden FILE __stderr_FILE = {
    .buf = errbuf + UNGET, .buf_size = 0, .fd = 2, .flags = F_PERM | F_NORD, .lbf = -1,
    .write = log_write, .seek = no_seek, .close = no_close, .lock = -1,
};
#undef stdin
#undef stdout
#undef stderr
FILE *const stdin = &__stdin_FILE;
FILE *const stdout = &__stdout_FILE;
FILE *const stderr = &__stderr_FILE;
FILE *volatile __stdin_used = &__stdin_FILE;
FILE *volatile __stdout_used = &__stdout_FILE;
FILE *volatile __stderr_used = &__stderr_FILE;

FILE *fopen(const char *path, const char *mode) {
    (void)path, (void)mode;
    errno = ENOENT;
    return NULL;
}
FILE *freopen(const char *path, const char *mode, FILE *f) {
    (void)path, (void)mode, (void)f;
    errno = ENOENT;
    return NULL;
}
FILE *tmpfile(void) {
    errno = ENOENT;
    return NULL;
}
FILE *popen(const char *c, const char *m) {
    (void)c, (void)m;
    errno = ENOENT;
    return NULL;
}
int pclose(FILE *f) {
    (void)f;
    return -1;
}
/* Only the three standard files exist, and they stay open. */
int fclose(FILE *f) {
    (void)f;
    return 0;
}
int fflush(FILE *f) {
    if (f && f->wpos != f->wbase) {
        f->write(f, 0, 0);
    }
    return 0;
}
/* musl's stdio internals: no FILE here is ever locked (lock -1). */
int __lockfile(FILE *f) {
    (void)f;
    return 0;
}
void __unlockfile(FILE *f) { (void)f; }
void __stdio_exit_needed(void) {}
int wctomb(char *s, wchar_t wc) {
    if (!s) {
        return 0;
    }
    if ((unsigned)wc < 0x80) {
        *s = (char)wc;
        return 1;
    }
    errno = EILSEQ;
    return -1;
}
/* dlmalloc's spin lock yields here. */
int sched_yield(void) { return 0; }

/* Asked for by musl's code paths that never run here: no locale, no file
 * is ever contended (locks are -1), and no FILE is ever read with %ls. */
int strcoll(const char *a, const char *b) { return strcmp(a, b); }
int fscanf(FILE *f, const char *fmt, ...) {
    (void)f, (void)fmt;
    return EOF;
}
pid_t gettid(void) { return 1; }
int emscripten_futex_wake(volatile void *addr, int count) {
    (void)addr, (void)count;
    return 0;
}

/* sprintf as Microsoft's C runtime (which LuaJIT's lua51.dll formats
 * numbers with) writes exponents: at least three digits ("1e+020"), where
 * musl writes two. Lua formats through sprintf only (tostring, %.14g, and
 * string.format one conversion at a time). */
static int msvc_exponents(char *s, int n) {
    for (int i = 0; i + 3 <= n; ++i) {
        if ((s[i] == 'e' || s[i] == 'E') && (s[i + 1] == '+' || s[i + 1] == '-') && s[i + 2] >= '0' && s[i + 2] <= '9') {
            int d = 0;
            while (i + 2 + d < n && s[i + 2 + d] >= '0' && s[i + 2 + d] <= '9') {
                ++d;
            }
            if (d == 2) {
                memmove(s + i + 3, s + i + 2, (size_t)(n - (i + 2)) + 1);
                s[i + 2] = '0';
                ++n;
            }
            break;
        }
    }
    return n;
}
int sprintf(char *s, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vsprintf(s, fmt, ap);
    va_end(ap);
    const char *c = strpbrk(fmt, "eEgG");
    return (c && n > 0) ? msvc_exponents(s, n) : n;
}
int siprintf(char *s, const char *fmt, ...) __attribute__((alias("sprintf")));
int __small_sprintf(char *s, const char *fmt, ...) __attribute__((alias("sprintf")));
