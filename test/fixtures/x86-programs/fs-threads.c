/* The working directory belongs to the fs_struct, not the thread, and the
 * path calls run without the kernel lock. pthreads share their creator's fs_struct: a chdir in one is seen
 * by its siblings. unshare(CLONE_FS) gives a thread its own, which the
 * threads it then creates share and the rest of the process does not see,
 * and which an execve from that thread keeps. While threads open and stat
 * through a directory descriptor, others rename in it: files back and
 * forth, a directory with a file in it back and forth, and the descriptor's
 * own directory back and forth. Every open of a file finds one of its two
 * names or neither and what it opens is the file; a directory renamed takes
 * its file along; and a descriptor follows its own directory, so an open
 * through it never misses. The working directory is held the same way: renamed, getcwd says where it went, and relative opens racing
 * renames of it never miss either. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
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

/* --- execve keeps the caller's fs_struct ---------------------------- */

static char *self;
static int hold[2], up[2];

/* A thread that stays on the process's first fs_struct until the exec. */
static void *bystander(void *arg) {
    (void)arg;
    char c = 1;
    (void)!write(up[1], &c, 1);
    (void)!read(hold[0], &c, 1);
    return NULL;
}

/* --- *at calls racing renames -------------------------------------- */

static int check(int fd, const char *want);

static int dfd, efd;
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

/* A directory with a file in it, renamed back and forth in d/. */
static void *dir_renamer(void *arg) {
    (void)arg;
    for (int i = 0; !stop; i++) {
        if (renameat(dfd, i & 1 ? "s2" : "s1", dfd, i & 1 ? "s1" : "s2")) fail("rename a directory");
        if (i % 16 == 0) sched_yield();
    }
    return NULL;
}

/* The directory efd is open on, renamed back and forth under the root. */
static void *own_renamer(void *arg) {
    (void)arg;
    char a[96], b[96];
    snprintf(a, sizeof a, "%s/e1", root);
    snprintf(b, sizeof b, "%s/e2", root);
    for (int i = 0; !stop; i++) {
        if (rename(i & 1 ? b : a, i & 1 ? a : b)) fail("rename the descriptor's directory");
        if (i % 16 == 0) sched_yield();
    }
    return NULL;
}

/* The working directory, renamed back and forth under the root. */
static void *cwd_renamer(void *arg) {
    (void)arg;
    char a[96], b[96];
    snprintf(a, sizeof a, "%s/w1", root);
    snprintf(b, sizeof b, "%s/w2", root);
    for (int i = 0; i < ROUNDS * 4; i++) {
        if (rename(i & 1 ? b : a, i & 1 ? a : b)) fail("rename the working directory");
        if (i % 16 == 0) sched_yield();
    }
    stop = 1;
    return NULL;
}

/* Relative to the working directory being renamed: always there. */
static void *cwd_opener(void *arg) {
    (void)arg;
    while (!stop) {
        int fd = open("here", O_RDONLY);
        if (fd < 0) fail("a relative open while the working directory is renamed");
        else if (!check(fd, "here")) fail("opened something else relative to a renamed cwd");
        struct stat st;
        if (stat("here", &st) || st.st_size != 4) fail("a relative stat while the working directory is renamed");
        char cwd[256];
        if (!getcwd(cwd, sizeof cwd) || !(ends_with(cwd, "/w1") || ends_with(cwd, "/w2"))) fail("getcwd while the working directory is renamed");
    }
    return NULL;
}

static int check(int fd, const char *want) {
    char buf[16] = {0};
    size_t n = strlen(want);
    int ok = read(fd, buf, sizeof buf - 1) == (ssize_t)n && !strcmp(buf, want);
    close(fd);
    return ok;
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
            if (!check(fd, "the file")) fail("opened something else");
            found++;
            struct stat st;
            if (fstatat(dfd, n, &st, 0) == 0 && st.st_size != 8) fail("fstatat size");
        }
        /* The file in the directory being renamed: under one name or the
         * other, never left behind under both or neither for long. */
        for (const char *n = "s1/in"; n; n = n[1] == '1' ? "s2/in" : NULL) {
            int fd = openat(dfd, n, O_RDONLY);
            if (fd < 0) {
                if (errno != ENOENT) fail("openat in a renamed directory: not ENOENT");
                continue;
            }
            if (!check(fd, "inside")) fail("opened something else in a renamed directory");
        }
        /* Through the descriptor whose own directory is being renamed: it
         * follows the directory, so the file is always there. */
        int fd = openat(efd, "own", O_RDONLY);
        if (fd < 0) fail("openat through a descriptor whose directory was renamed");
        else if (!check(fd, "own")) fail("opened something else through a renamed directory");
        struct stat st;
        if (fstatat(efd, "own", &st, 0) || st.st_size != 3) fail("fstatat through a renamed directory");
    }
    if (!found) fail("never found either name");
    return NULL;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "exec-cwd")) {
        char cwd[256];
        return getcwd(cwd, sizeof cwd) && ends_with(cwd, "/b") ? 0 : 7;
    }
    /* Absolute: the exec runs from another directory. */
    static char me[512];
    if (argv[0][0] == '/') snprintf(me, sizeof me, "%s", argv[0]);
    else if (getcwd(me, 256)) snprintf(me + strlen(me), sizeof me - strlen(me), "/%s", argv[0]);
    self = me;
    /* Its own directory: concurrent runs under gexc share /tmp, and the
     * guest's pid is the same in each. */
    snprintf(root, sizeof root, "/tmp/fs-threads-XXXXXX");
    if (!mkdtemp(root)) return 2;
    chmod(root, 0755);
    char p[128];
    snprintf(p, sizeof p, "%s/a", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/b", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/b/c", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/d", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/a/only-in-a", root); put(p, "a");
    snprintf(p, sizeof p, "%s/b/c/only-in-c", root); put(p, "c");
    snprintf(p, sizeof p, "%s/d/f", root); put(p, "the file");
    snprintf(p, sizeof p, "%s/d/s1", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/d/s1/in", root); put(p, "inside");
    snprintf(p, sizeof p, "%s/e1", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/e1/own", root); put(p, "own");
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

    /* A forked child (in a/) whose main thread takes an fs_struct of its
     * own, apart from its other thread's, moves to b/ and execs: the new
     * image starts in b/, the caller's directory, not the one the rest of
     * the process had. (From the main thread: gexc runs an execve from no
     * other.) */
    pid_t c = fork();
    if (c == 0) {
        pthread_t t;
        char ready;
        if (pipe(hold) || pipe(up)) _exit(4);
        pthread_create(&t, NULL, bystander, NULL);
        /* Running before the exec (an exec racing its start is another
         * matter). */
        if (read(up[0], &ready, 1) != 1) _exit(4);
        if (unshare(CLONE_FS) || chdir("../b")) _exit(5);
        char *av[] = {self, "exec-cwd", NULL};
        execv(self, av);
        _exit(6);
    }
    int status = 0;
    if (c < 0 || waitpid(c, &status, 0) != c || !WIFEXITED(status) || WEXITSTATUS(status)) {
        errno = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
        fail("execve from a thread with its own fs_struct: not in its directory");
    }

    snprintf(p, sizeof p, "%s/d", root);
    dfd = open(p, O_RDONLY | O_DIRECTORY);
    snprintf(p, sizeof p, "%s/e1", root);
    efd = open(p, O_RDONLY | O_DIRECTORY);
    if (dfd < 0 || efd < 0) return 1;
    pthread_t r, dr, er, o[3];
    for (int i = 0; i < 3; i++) pthread_create(&o[i], NULL, opener, NULL);
    pthread_create(&dr, NULL, dir_renamer, NULL);
    pthread_create(&er, NULL, own_renamer, NULL);
    pthread_create(&r, NULL, renamer, NULL);
    pthread_join(r, NULL);
    pthread_join(dr, NULL);
    pthread_join(er, NULL);
    for (int i = 0; i < 3; i++) pthread_join(o[i], NULL);
    /* Wherever the renames left them, the descriptors still name them. */
    if (fchdir(efd)) fail("fchdir to a renamed directory");
    if (access("own", R_OK)) fail("a relative lookup after fchdir to a renamed directory");

    /* The working directory renamed: getcwd follows it, as on Linux. */
    snprintf(p, sizeof p, "%s/w1", root); mkdir(p, 0755);
    snprintf(p, sizeof p, "%s/w1/here", root); put(p, "here");
    if (chdir(root) || chdir("w1")) fail("chdir w1");
    char w2[96];
    snprintf(p, sizeof p, "%s/w1", root);
    snprintf(w2, sizeof w2, "%s/w2", root);
    if (rename(p, w2)) fail("rename the working directory");
    if (!getcwd(cwd, sizeof cwd) || !ends_with(cwd, "/w2")) fail("getcwd after a rename of the working directory");
    if (access("here", R_OK)) fail("a relative lookup after a rename of the working directory");
    if (rename(w2, p)) fail("rename it back");
    stop = 0;
    pthread_t cr, co[3];
    for (int i = 0; i < 3; i++) pthread_create(&co[i], NULL, cwd_opener, NULL);
    pthread_create(&cr, NULL, cwd_renamer, NULL);
    pthread_join(cr, NULL);
    for (int i = 0; i < 3; i++) pthread_join(co[i], NULL);
    /* Where the renames left it: w1 (an even count). */
    if (!getcwd(cwd, sizeof cwd) || !ends_with(cwd, "/w1")) fail("getcwd after the renames");
    if (chdir(root)) return 1;
    snprintf(p, sizeof p, "%s/w1/here", root); unlink(p);
    snprintf(p, sizeof p, "%s/w1", root); rmdir(p);

    snprintf(p, sizeof p, "%s/d/f", root); unlink(p);
    snprintf(p, sizeof p, "%s/d/g", root); unlink(p);
    for (const char *n = "s1s2"; *n; n += 2) {
        snprintf(p, sizeof p, "%s/d/%.2s/in", root, n); unlink(p);
        snprintf(p, sizeof p, "%s/d/%.2s", root, n); rmdir(p);
    }
    for (const char *n = "e1e2"; *n; n += 2) {
        snprintf(p, sizeof p, "%s/%.2s/own", root, n); unlink(p);
        snprintf(p, sizeof p, "%s/%.2s", root, n); rmdir(p);
    }
    snprintf(p, sizeof p, "%s/a/only-in-a", root); unlink(p);
    snprintf(p, sizeof p, "%s/b/c/only-in-c", root); unlink(p);
    snprintf(p, sizeof p, "%s/b/c", root); rmdir(p);
    for (const char *d = "abd"; *d; d++) { snprintf(p, sizeof p, "%s/%c", root, *d); rmdir(p); }
    rmdir(root);
    printf("bad %ld\n", bad);
    return bad != 0;
}
