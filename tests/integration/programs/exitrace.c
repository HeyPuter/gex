/* The process ends while other threads are still working.
 * exit_group must take them down; nothing may hang or crash, and the exit
 * status is main's.
 */
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
static volatile long spins;
static void *spin(void *p) {
    (void)p;
    for (;;) { spins++; }           /* never returns: only exit_group stops it */
    return NULL;
}
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
static void *parked(void *p) {
    (void)p;
    pthread_mutex_lock(&m);
    while (1) pthread_cond_wait(&c, &m);  /* parked on a futex forever */
    return NULL;
}
int main(void) {
    pthread_t t;
    for (int i = 0; i < 3; i++) pthread_create(&t, NULL, spin, NULL);
    for (int i = 0; i < 2; i++) pthread_create(&t, NULL, parked, NULL);
    usleep(50000);
    printf("main exiting with 3 spinners and 2 parked\n");
    fflush(stdout);
    return 7;
}
