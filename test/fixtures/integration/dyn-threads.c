/* Threads reaching *different* untranslated library code at the same moment,
 * so their first dispatch misses overlap and race the translator's slot
 * allocation. Each thread starts in a different corner of libc.
 *
 * Correctness is checked by rerunning the same work afterwards on one thread,
 * everything already translated: a thread that ran the wrong code disagrees
 * even if it did not crash.
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
