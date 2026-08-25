/* memcpy and memset, at the sizes real programs use them.
 *
 * The point of the size mix: a compressor's `memcpy` is mostly small -- a
 * match of eight or sixteen bytes, copied a million times -- while a buffer
 * shuffle is one large one. `memory.copy` is a single wasm instruction, but
 * an engine may implement it as a call into its runtime, which a translated
 * eight-byte loop beats easily. So the answer to "should `memcpy` become
 * `memory.copy`" depends entirely on the distribution, and this measures it.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define BUF (1 << 20)

static uint8_t a[BUF], b[BUF];

int main(int argc, char **argv) {
    long reps = argc > 1 ? atol(argv[1]) : 200;
    for (int i = 0; i < BUF; i++) a[i] = (uint8_t)(i * 7 + 1);
    uint64_t h = 0;
    /* The mix: mostly small, a few large, as a match-copying loop looks. */
    static const int sizes[] = {8, 16, 8, 24, 8, 32, 12, 8, 64, 8, 16, 4096, 8, 8, 128, 8};
    for (long r = 0; r < reps; r++) {
        size_t off = 0;
        for (int k = 0; k < 4096; k++) {
            int n = sizes[k & 15];
            if (off + n + 4096 >= BUF) off = 0;
            memcpy(b + off, a + off + 1, n);
            off += n;
        }
        memset(b + (r & 0xffff), (int)(r & 0xff), 1 << 14);
        h ^= b[(r * 37) & (BUF - 1)] + (h << 3);
    }
    printf("%llu\n", (unsigned long long)h);
    return 0;
}
