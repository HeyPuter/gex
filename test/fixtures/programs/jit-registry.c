/* registry: the guest of test/page/registry.test.js. */
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#define N 3000
/* Salted per run, so no cache of an earlier run's translations serves them. */
static int salt;
static long expect(int i, long x) {
    for (int k = 0; k < 40; k++) {
        int v = (i * 131 + k) ^ salt;
        x += v;
        x ^= (long)(v ^ 0x5a5a);
    }
    return x;
}
static void *idle(void *a) { return a; }
static void *check(void *a) {
    /* A thread that joins after the functions exist: catches up on them all. */
    long bad = 0;
    unsigned char *code = a;
    for (int i = 0; i < N; i += 97) bad += ((long (*)(long))(code + (long)i * 4096))(i) != expect(i, i);
    return (void *)bad;
}
int main(void) {
    pthread_t t;
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    salt = (int)(ts.tv_nsec ^ getpid() << 8) & 0x3fffffff;
    /* Threaded first: only then does the registry keep anything. */
    pthread_create(&t, 0, idle, 0);
    pthread_join(t, 0);
    unsigned char *code = mmap(0, (size_t)N * 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED) { perror("mmap"); return 1; }
    long bad = 0;
    for (int i = 0; i < N; i++) {
        unsigned char *f = code + (long)i * 4096, *p = f;
        /* mov rax, rdi; then 40 x (add rax, imm32; xor rax, imm32); ret */
        *p++ = 0x48; *p++ = 0x89; *p++ = 0xf8;
        for (int k = 0; k < 40; k++) {
            int v = (i * 131 + k) ^ salt;
            *p++ = 0x48; *p++ = 0x05; memcpy(p, &v, 4); p += 4;
            *p++ = 0x48; *p++ = 0x35; v ^= 0x5a5a; memcpy(p, &v, 4); p += 4;
        }
        *p++ = 0xc3;
        /* Often enough to be translated, not interpreted. */
        for (int k = 0; k < 64; k++) bad += ((long (*)(long))f)(i + k) != expect(i, i + k);
    }
    void *r;
    pthread_create(&t, 0, check, code);
    pthread_join(t, &r);
    printf("%d functions, %ld wrong, %ld wrong in the joining thread\n", N, bad, (long)r);
    return 0;
}
