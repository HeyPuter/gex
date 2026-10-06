/* The first thread ends with exit (pthread_exit, not exit_group) while
 * another runs: the process goes on until that one ends it, with its code
 * (Linux; Wine's ExitProcess from a second thread does exactly this). */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void *worker(void *arg)
{
    (void)arg;
    usleep(50000);
    printf("worker still running\n");
    fflush(stdout);
    exit(33);
}

int main(void)
{
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    printf("main thread exiting\n");
    fflush(stdout);
    pthread_exit(NULL);
}
