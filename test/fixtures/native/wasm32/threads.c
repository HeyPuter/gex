/* wasm32 programs under gex: threads (a module instance per thread),
 * thread-locals, the heap, stat, and a child process by posix_spawn. */
#define _GNU_SOURCE
#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static _Thread_local int tl = 1;
static int counter;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;

static void *thr(void *a)
{
    tl += (int)(long)a;
    for (int i = 0; i < 10000; i++) {
        pthread_mutex_lock(&mu);
        counter++;
        pthread_mutex_unlock(&mu);
    }
    return (void *)(long)(tl * 10);
}

int main(int argc, char **argv)
{
    char *p = malloc(100000);
    strcpy(p, "heap ok");
    puts(p);
    struct stat st;
    printf("stat / is a directory: %d\n", stat("/", &st) == 0 && S_ISDIR(st.st_mode));
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], 0, thr, (void *)(i + 5));
    for (int i = 0; i < 4; i++) {
        void *r;
        pthread_join(t[i], &r);
        printf("thread %d -> %ld\n", i, (long)r);
    }
    printf("main tl %d, counter %d\n", tl, counter);
    fflush(stdout);
    if (argc > 1) return 3;
    extern char **environ;
    char *cargv[] = { argv[0], "child", NULL };
    pid_t pid;
    if (posix_spawn(&pid, argv[0], NULL, NULL, cargv, environ)) return 1;
    int ws;
    waitpid(pid, &ws, 0);
    printf("child exit %d\n", WEXITSTATUS(ws));
    return 0;
}
