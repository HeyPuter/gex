/* Loops of every shape, checked by their results.
 *
 * The rest of the suite is mostly straight-line: it exercises instructions,
 * and a loop only appears as scaffolding. That left a hole. A change to how
 * loops are translated -- emitting a whole loop as one function, say -- can
 * pass every other program here and still lose six rows out of three hundred
 * thousand in SQLite, which is how this file came to exist.
 *
 * Everything below reduces to one number per shape, so a wrong iteration
 * count or a register that did not survive a back edge shows up as a
 * different number rather than as a crash.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define N 4096

static uint64_t mix(uint64_t h, uint64_t v) {
    h ^= v + 0x9e3779b97f4a7c15ULL + (h << 6) + (h >> 2);
    return h;
}

/* Counted loop, one basic block. */
static uint64_t counted(void) {
    uint64_t h = 1;
    for (int i = 0; i < 100000; i++) h = mix(h, i);
    return h;
}

/* Two blocks: a test at the top and a body that falls back to it. */
static uint64_t conditional(void) {
    uint64_t h = 2;
    int i = 0;
    while (i < 100000) {
        if (i % 3 == 0) h = mix(h, i);
        else h += i;
        i++;
    }
    return h;
}

/* Nested, with the inner trip count depending on the outer index. */
static uint64_t nested(void) {
    uint64_t h = 3;
    for (int i = 1; i <= 400; i++)
        for (int j = 0; j < i; j++) h = mix(h, (uint64_t)i * j);
    return h;
}

/* Early exit from the middle, and a continue. */
static uint64_t early_exit(void) {
    uint64_t h = 4;
    for (int i = 0; i < 100000; i++) {
        if ((i & 1023) == 777) continue;
        h = mix(h, i);
        if (h % 1000003 == 0) break;
    }
    return h;
}

/* do/while: the test is at the bottom, so the back edge is the only branch. */
static uint64_t bottom_test(void) {
    uint64_t h = 5;
    int i = 0;
    do {
        h = mix(h, i);
        i += 3;
    } while (i < 100000);
    return h;
}

/* A loop whose body calls out, so it cannot be one straight-line region. */
static uint64_t with_calls(void) {
    uint64_t h = 6;
    for (int i = 0; i < 20000; i++) h = mix(h, mix(h ^ i, i * 7u));
    return h;
}

/* Pointer walking with a data-dependent branch, the shape a sort has. */
static uint64_t sort_and_sum(void) {
    static long a[N];
    for (int i = 0; i < N; i++) a[i] = (long)((i * 2654435761u) % 1000003u);
    /* Insertion sort: nested loops with a data-dependent inner trip count. */
    for (int i = 1; i < N; i++) {
        long v = a[i];
        int j = i - 1;
        while (j >= 0 && a[j] > v) {
            a[j + 1] = a[j];
            j--;
        }
        a[j + 1] = v;
    }
    uint64_t h = 7;
    for (int i = 0; i < N; i++) {
        if (i && a[i] < a[i - 1]) return 0; /* not sorted */
        h = mix(h, (uint64_t)a[i]);
    }
    return h;
}

/* Every element visited exactly once, so a lost iteration changes the count. */
static uint64_t exact_count(void) {
    static unsigned char seen[N];
    memset(seen, 0, sizeof seen);
    unsigned n = 0;
    for (unsigned i = 0; i < N; i++) {
        unsigned k = (i * 7919u) % N;
        while (seen[k]) k = (k + 1) % N;
        seen[k] = 1;
        n++;
    }
    unsigned missing = 0;
    for (int i = 0; i < N; i++)
        if (!seen[i]) missing++;
    return ((uint64_t)n << 32) | missing;
}

int main(void) {
    printf("counted=%016llx\n", (unsigned long long)counted());
    printf("conditional=%016llx\n", (unsigned long long)conditional());
    printf("nested=%016llx\n", (unsigned long long)nested());
    printf("early-exit=%016llx\n", (unsigned long long)early_exit());
    printf("bottom-test=%016llx\n", (unsigned long long)bottom_test());
    printf("with-calls=%016llx\n", (unsigned long long)with_calls());
    printf("sorted=%016llx\n", (unsigned long long)sort_and_sum());
    printf("exact=%016llx\n", (unsigned long long)exact_count());
    return 0;
}
