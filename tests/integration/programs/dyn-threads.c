/* Threads reaching *different* untranslated library code at the same moment.
 *
 * Only the executable is translated ahead of time; every shared library
 * arrives through the runtime translator, one dispatch miss at a time. A miss
 * hands the missing page a base slot in the dispatch table by bumping a
 * counter -- and that counter used to be read, spent milliseconds of lifting
 * and seconds of Cranelift, and only then written back. Two threads missing at
 * once both allocated from the same mark, so two different guest pages were
 * given the same slots, and an indirect branch to one of them landed in the
 * middle of a function belonging to the other.
 *
 * dpkg's xz decompressor found it: an `exit` handler returned into the wrong
 * code and failed its stack canary, having already written a correct 5 MB of
 * output.
 *
 * Threads all running the same function miss the same pages, and the first one
 * to arrive translates for everybody, so each thread here starts in a
 * different corner of libc and the misses land on different pages.
 *
 * Being honest about what this does not do: it did not reproduce the bug. With
 * the fix reverted, eight runs of this passed and every run of the
 * decompressor failed. musl's libc is small enough that six threads still do
 * not overlap two translations often, where glibc plus liblzma does it
 * reliably. What actually guards the invariant is the check in the host's
 * `publish_unit`, which notices the first base slot claimed by two guest pages
 * and says so; this is a concurrency test that happens to cover the same
 * ground, not the thing standing between the bug and a release.
 *
 * Correctness is checked by running the same work again afterwards, on one
 * thread, with everything already translated: a thread that ran the wrong code
 * disagrees even if it does not crash.
 */
#define _GNU_SOURCE
#include <ctype.h>
#include <math.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wchar.h>

#define THREADS 6
#define ROUNDS 400

static int bytecmp(const void *a, const void *b) {
    return *(const unsigned char *)a - *(const unsigned char *)b;
}

static unsigned long mix(unsigned long h, unsigned long v) {
    return (h ^ v) * 1099511628211UL;
}

/* Six disjoint corners of libc. Each `kind` should reach code the others do
   not, so that six threads starting together miss on six sets of pages. */
static unsigned long corner(int kind, unsigned seed) {
    unsigned long h = 1469598103934665603UL;
    char buf[256], tmp[256];
    for (int i = 0; i < ROUNDS; i++) {
        seed = seed * 1103515245u + 12345u;
        switch (kind) {
        case 0: /* formatted output */
            snprintf(buf, sizeof buf, "%u|%x|%o|%+d|%%", seed, seed,
                     seed & 0777, (int)seed);
            h = mix(h, strlen(buf));
            break;
        case 1: /* floating point formatting and parsing */
            snprintf(buf, sizeof buf, "%.9e %.6f %g", (double)seed / 3.0,
                     (double)(seed % 100000) / 7.0, (double)seed * 1e-9);
            h = mix(h, (unsigned long)(strtod(buf, NULL) * 16.0));
            break;
        case 2: /* the string and memory routines */
            memset(buf, 'a' + (int)(seed % 26), sizeof buf - 1);
            buf[sizeof buf - 1] = 0;
            buf[seed % (sizeof buf - 1)] = 'Z';
            h = mix(h, (unsigned long)(strchr(buf, 'Z') - buf));
            h = mix(h, (unsigned long)strcspn(buf, "Z"));
            memmove(tmp, buf, sizeof tmp);
            h = mix(h, (unsigned long)memcmp(tmp, buf, sizeof tmp));
            break;
        case 3: /* the maths library */
            h = mix(h, (unsigned long)(sqrt((double)(seed % 99991) + 1.0) * 64));
            h = mix(h, (unsigned long)(pow(1.000001, (double)(seed % 4096)) * 1e6));
            h = mix(h, (unsigned long)(fabs(sin((double)(seed % 6283) / 1000.0)) * 1e6));
            h = mix(h, (unsigned long)(log((double)(seed % 65521) + 2.0) * 1e6));
            break;
        case 4: /* allocation and sorting */
        {
            size_t n = 16 + seed % 200;
            unsigned char *p = malloc(n);
            for (size_t j = 0; j < n; j++) {
                p[j] = (unsigned char)(seed >> (j % 24));
            }
            qsort(p, n, 1, bytecmp);
            h = mix(h, p[0] * 256u + p[n - 1]);
            char *d = strndup((char *)p, 8);
            h = mix(h, strlen(d));
            free(d);
            free(p);
            break;
        }
        default: /* time, wide characters and character classes */
        {
            time_t t = (time_t)(seed % 1000000);
            struct tm g;
            gmtime_r(&t, &g);
            strftime(buf, sizeof buf, "%Y-%m-%dT%H:%M:%S", &g);
            h = mix(h, strlen(buf));
            wchar_t w[64];
            h = mix(h, (unsigned long)mbstowcs(w, buf, 64));
            for (const char *p = buf; *p; p++) {
                h = mix(h, (unsigned long)(isdigit((unsigned char)*p) +
                                           2 * isalpha((unsigned char)*p)));
            }
            break;
        }
        }
    }
    return h;
}

struct arg {
    int kind;
    unsigned seed;
    unsigned long out;
};

static void *run(void *p) {
    struct arg *a = p;
    a->out = corner(a->kind, a->seed);
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    pthread_t t[THREADS];
    struct arg a[THREADS];

    /* Created before any of them has run, so their first misses overlap. */
    for (int i = 0; i < THREADS; i++) {
        a[i].kind = i;
        a[i].seed = 1u + (unsigned)i * 7919u;
        if (pthread_create(&t[i], NULL, run, &a[i]) != 0) {
            perror("pthread_create");
            return 1;
        }
    }
    for (int i = 0; i < THREADS; i++) {
        pthread_join(t[i], NULL);
    }
    for (int i = 0; i < THREADS; i++) {
        printf("corner %d: %016lx\n", a[i].kind, a[i].out);
    }
    for (int i = 0; i < THREADS; i++) {
        unsigned long again = corner(a[i].kind, a[i].seed);
        if (again != a[i].out) {
            printf("corner %d disagrees: %016lx vs %016lx\n", a[i].kind,
                   a[i].out, again);
            return 1;
        }
    }
    printf("all %d corners agree\n", THREADS);
    return 0;
}
