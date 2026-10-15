/* The working directory belongs to the fs_struct, not the thread, and the
 * path calls run without the kernel lock (docs/kernel-locking.md, L14b).
 * pthreads share their creator's fs_struct: a chdir in one is seen by its
 * siblings. unshare(CLONE_FS) gives a thread its own, which the threads it
 * then creates share and the rest of the process does not see. While
 * threads open, stat and readlink through a directory descriptor, another
 * renames the files in that directory back and forth: every open finds one
 * of the two names or neither, and what it opens is the file. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define ROUNDS 400
static long bad;
static char root[64];

static void fail(const char *what) {
    if (__atomic_add_fetch(&bad, 1, __ATOMIC_RELAXED) < 5) fprintf(stderr, "bad: %s (errno %d)\n", what, errno);
}

static int ends_with(const char *s, const char *tail) {
    size_t a = strlen(s), b = strlen(tail);
    return a >= b && !strcmp(s + a - b, tail);
}

static void put(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text)) fail("put");
    close(fd);
}

/* --- a chdir seen by the siblings ---------------------------------- */

static pthread_barrier_t moved;

static void *mover(void *arg) {
    (void)arg;
    if (chdir("a")) fail("chdir a");
    pthread_barrier_wait(&moved);
    return NULL;
}

static void *sibling(void *arg) {
    (void)arg;
    pthread_barrier_wait(&moved);
    char cwd[256];
    if (!getcwd(cwd, sizeof cwd) || !ends_with(cwd, "/a")) fail("a sibling's cwd after the chdir");
    /* Relative opens and stats resolve in the new directory. */
    int fd = open("only-in-a", O_RDONLY);
    if (fd < 0) fail("a relative open after a sibling's chdir");
    else close(fd);
    struct stat st;
    if (stat("only-in-a", &st) || access("only-in-a", R_OK)) fail("a relative stat after a sibling's chdir");
    return NULL;
}

/* --- unshare(CLONE_FS) ---------------------------------------------- */

static pthread_barrier_t inner_done;

static void *shares_own(void *arg) {
    (void)arg;
    char cwd[256];
    /* Its creator's fs_struct, the private one. */
    if (!getcwd(cwd, sizeof cwd) || !ends_with(cwd, "/b")) fail("a thread made after unshare: not its creator's cwd");
    if (chdir("c")) fail("chdir c");
    pthread_barrier_wait(&inner_done);
    return NULL;
}

static void *unsharer(void *arg) {
    (void)arg;
    if (unshare(CLONE_FS)) { fail("unshare"); return NULL; }
    if (chdir("../b")) fail("chdir ../b");
    pthread_barrier_init(&inner_done, NULL, 2);
    pthread_t t;
    pthread_create(&t, NULL, shares_own, NULL);
    pthread_barrier_wait(&inner_done);
    pthread_join(t, NULL);
    char cwd[256];
    /* The thread it made changed the directory they share. */
    if (!getcwd(cwd, sizeof cwd) || !ends_with(cwd, "/b/c")) fail("the shared private fs_struct");
    int fd = open("only-in-c", O_RDONLY);
    if (fd < 0) fail("a relative open in the private directory");
    else close(fd);
    return NULL;
}

/* --- *at calls racing a rename ------------------------------------- */

static int dfd;
static volatile int stop;

static void *renamer(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS * 4; i++) {
        if (renameat(dfd, i & 1 ? "g" : "f", dfd, i & 1 ? "f" : "g")) fail("rename");
        if (i % 16 == 0) sched_yield();
    }
    stop = 1;
    return NULL;
}

static void *opener(void *arg) {
    (void)arg;
    long found = 0;
    while (!stop) {
        for (const char *n = "f"; n; n = *n == 'f' ? "g" : NULL) {
            int fd = openat(dfd, n, O_RDONLY);
            if (fd < 0) {
                if (errno != ENOENT) fail("openat: not ENOENT");
                continue;
            }
            char buf[16] = {0};
            if (read(fd, buf, sizeof buf - 1) != 8 || strcmp(buf, "the file")) fail("opened something else");
            close(fd);
            found++;
            struct stat st;
            if (fstatat(dfd, n, &st, 0) == 0 && st.st_size != 8) fail("fstatat size");
        }
    }
    if (!found) fail("never found either name");
    return NULL;
}

int main(void) {
    snprintf(root, sizeof root, "/tmp/fs-threads-%d", (int)getpid());
    char p[128];
    mkdir(root, 0755);
    snprintf(p, sizeof p, "%s/a", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/b", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/b/c", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/d", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/a/only-in-a", root); put(p, "a");
    snprintf(p, sizeof p, "%s/b/c/only-in-c", root); put(p, "c");
    snprintf(p, sizeof p, "%s/d/f", root); put(p, "the file");
    if (chdir(root)) return 1;

    pthread_barrier_init(&moved, NULL, 3);
    pthread_t m, s1, s2;
    pthread_create(&m, NULL, mover, NULL);
    pthread_create(&s1, NULL, sibling, NULL);
    pthread_create(&s2, NULL, sibling, NULL);
    pthread_join(m, NULL);
    pthread_join(s1, NULL);
    pthread_join(s2, NULL);
    char cwd[256];
    if (!getcwd(cwd, sizeof cwd) || !ends_with(cwd, "/a")) fail("the main thread's cwd after a pthread's chdir");

    /* From a/: a thread that unshares sees its own; the rest stay in a/. */
    pthread_t u;
    pthread_create(&u, NULL, unsharer, NULL);
    pthread_join(u, NULL);
    if (!getcwd(cwd, sizeof cwd) || !ends_with(cwd, "/a")) fail("the process's cwd after a private chdir");
    if (access("only-in-a", R_OK)) fail("relative lookups after a private chdir");

    snprintf(p, sizeof p, "%s/d", root);
    dfd = open(p, O_RDONLY | O_DIRECTORY);
    if (dfd < 0) return 1;
    pthread_t r, o[3];
    for (int i = 0; i < 3; i++) pthread_create(&o[i], NULL, opener, NULL);
    pthread_create(&r, NULL, renamer, NULL);
    pthread_join(r, NULL);
    for (int i = 0; i < 3; i++) pthread_join(o[i], NULL);

    snprintf(p, sizeof p, "%s/d/f", root); unlink(p);
    snprintf(p, sizeof p, "%s/d/g", root); unlink(p);
    snprintf(p, sizeof p, "%s/a/only-in-a", root); unlink(p);
    snprintf(p, sizeof p, "%s/b/c/only-in-c", root); unlink(p);
    snprintf(p, sizeof p, "%s/b/c", root); rmdir(p);
    for (const char *d = "abd"; *d; d++) { snprintf(p, sizeof p, "%s/%c", root, *d); rmdir(p); }
    rmdir(root);
    printf("bad %ld\n", bad);
    return bad != 0;
}
