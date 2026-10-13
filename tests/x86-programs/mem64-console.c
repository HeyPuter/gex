/* The console written from above 4 GiB (docs/mem64.md): a thread's line
 * from its own stack, and a line from an anonymous mapping, each written
 * with write(2) straight from where it is. Under `gexc run --mem64` both are
 * in the high arena. In the page, stdout and stderr are the same console
 * device, whose bytes cross to the host by address: an address cut to 32
 * bits printed them as NUL bytes from the wrong place (a thread's stderr in
 * close-while-reading). Natively, or with a wasm32 memory, the output is the
 * same. x86-64 only. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static void *say(void *arg) {
    char line[64];
    int n = snprintf(line, sizeof line, "thread %ld: from its stack\n", (long)arg);
    if (write(1, line, n) != n) return (void *)1;
    return 0;
}

int main(void) {
    char *a = mmap(NULL, 1 << 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (a == MAP_FAILED) { puts("mmap failed"); return 1; }
    int n = snprintf(a, 1 << 16, "mapping: from anonymous memory\n");
    if (write(1, a, n) != n) return 1;
    /* One at a time, so the lines come out in order. */
    for (long i = 0; i < 4; i++) {
        pthread_t t;
        void *r;
        if (pthread_create(&t, 0, say, (void *)i)) { puts("pthread_create failed"); return 1; }
        pthread_join(t, &r);
        if (r) return 1;
    }
    return 0;
}
