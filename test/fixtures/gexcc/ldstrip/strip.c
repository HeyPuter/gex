#include <pthread.h>
#include <stdio.h>

static void *worker(void *arg) { return (char *)arg + 1; }

int main(int argc, char **argv) {
    pthread_t t;
    void *r;
    if (pthread_create(&t, 0, worker, argv[0]) || pthread_join(t, &r)) return 2;
    printf("stripped ok %d %d\n", argc, r == argv[0] + 1);
    return 0;
}
