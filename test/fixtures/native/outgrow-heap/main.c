/* outgrow-heap: the guest of test/native/outgrow-heap. Maps 128 MiB blocks
 * until the memory has no room for another, then:
 *
 *   outgrow-heap spin       asks for the same block again, as an allocator
 *                           that cannot go on without it does, up to 10000
 *                           times ("outgrow-heap still refused": gex let it
 *                           spin)
 *   outgrow-heap fallback   asks for half as much, and half again, until a
 *                           block fits ("outgrow-heap fallback ok")
 *
 * Says "outgrow-heap full" at the first refusal. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define BLOCK ((size_t)128 << 20)

static void *block(size_t len) {
    return mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "spin";
    int n = 0;

    while (block(BLOCK) != MAP_FAILED)
        if (++n > 1024) {
            printf("outgrow-heap never full\n");
            return 2;
        }
    printf("outgrow-heap full (%s)\n", errno == ENOMEM ? "ENOMEM" : strerror(errno));
    fflush(stdout);
    if (!strcmp(mode, "fallback")) {
        for (size_t len = BLOCK / 2; len >= 4096; len /= 2)
            if (block(len) != MAP_FAILED) {
                printf("outgrow-heap fallback ok\n");
                return 0;
            }
        printf("outgrow-heap fallback failed\n");
        return 1;
    }
    for (int i = 0; i < 10000; i++)
        if (block(BLOCK) != MAP_FAILED) {
            printf("outgrow-heap fits after all\n");
            return 1;
        }
    printf("outgrow-heap still refused\n");
    return 3;
}
