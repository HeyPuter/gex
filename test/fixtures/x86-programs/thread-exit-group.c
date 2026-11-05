/* exit() on a thread other than the first ends the process with its status,
 * while the first thread waits in pthread_join. gex lost the status and the
 * first thread waited for ever. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void *worker(void *arg) {
    (void)arg;
    usleep(50000);
    printf("worker exits\n");
    fflush(stdout);
    exit(3);
}

int main(void) {
    pthread_t t;
    pthread_create(&t, 0, worker, 0);
    pthread_join(t, 0);
    printf("joined a thread that ended the process\n");
    return 0;
}
