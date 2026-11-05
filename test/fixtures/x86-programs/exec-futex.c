/* Futexes after an exec: the program execs itself, and the new image's
 * threads contend for a mutex and wait on a condition variable. A host that
 * answers futex without the kernel lock checks the word against the memory
 * map it published; the exec's new map must be the one it checks (an old
 * one turned every wait into EFAULT, and the waiters spun). */
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
static long counter, turn;

static void *worker(void *arg) {
    long me = (long)arg;
    for (int i = 0; i < 2000; i++) {
        pthread_mutex_lock(&m);
        counter++;
        pthread_mutex_unlock(&m);
    }
    for (int i = 0; i < 200; i++) {
        pthread_mutex_lock(&m);
        while (turn % 2 != me) pthread_cond_wait(&c, &m);
        turn++;
        pthread_cond_broadcast(&c);
        pthread_mutex_unlock(&m);
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 2 || strcmp(argv[1], "after") != 0) {
        char *args[] = {argv[0], "after", NULL};
        execv(argv[0], args);
        perror("exec");
        return 1;
    }
    /* A wait on memory only the new image mapped times out; it is not
     * EFAULT. */
    int *w = mmap((void *)0x30000000, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (w == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    struct timespec ts = {0, 10000000};
    long r = syscall(SYS_futex, w, FUTEX_WAIT_PRIVATE, 0, &ts);
    printf("wait on new memory: %s\n", r == -1 && errno == ETIMEDOUT ? "timed out" : r == -1 && errno == EFAULT ? "EFAULT" : "other");
    pthread_t t[2];
    for (long i = 0; i < 2; i++) pthread_create(&t[i], NULL, worker, (void *)i);
    for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
    printf("after exec: counter %ld, turns %ld\n", counter, turn);
    return 0;
}
