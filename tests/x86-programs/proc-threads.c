/* /proc/self read from several threads while others change what it shows
 * (docs/kernel-locking.md, L14a): /proc reads the process's published state
 * live, not a snapshot refreshed before each syscall. A mapping a thread
 * just made is in its read of maps and gone after the unmap; a descriptor
 * it just opened is in fd/ and names the file; cmdline is argv, in a forked
 * child too, and the new argv after execve; a forked child's stat names
 * itself. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define ROUNDS 150
#define THREADS 3
static long bad;
static char tmpl[64];

static void fail(const char *what) {
    if (__atomic_add_fetch(&bad, 1, __ATOMIC_RELAXED) < 5) fprintf(stderr, "bad: %s\n", what);
}

static int slurp(const char *path, char *buf, int cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    int n = 0, r;
    while (n < cap - 1 && (r = read(fd, buf + n, cap - 1 - n)) > 0) n += r;
    close(fd);
    buf[n] = 0;
    return n;
}

/* Whether maps has a line for exactly [p, p + len). */
static int mapped(void *p, size_t len) {
    static __thread char buf[1 << 17];
    if (slurp("/proc/self/maps", buf, sizeof buf) <= 0) return -1;
    char want[48];
    snprintf(want, sizeof want, "%lx-%lx ", (unsigned long)p, (unsigned long)p + len);
    for (char *l = buf; l && *l; l = strchr(l, '\n'), l = l ? l + 1 : 0)
        if (!strncmp(l, want, strlen(want))) return 1;
    return 0;
}

/* Each thread's mappings are told apart by their protection (no other
 * mapping is read-only or read-and-execute alone, so nothing merges with
 * them) and their lengths (its own range of pages). */
static void *maps_thread(void *arg) {
    long id = (long)arg;
    int prot = id ? PROT_READ | PROT_EXEC : PROT_READ;
    for (int i = 0; i < ROUNDS; i++) {
        size_t len = 0x1000 * (1 + id * 5 + i % 5);
        void *p = mmap(NULL, len, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { fail("mmap"); continue; }
        if (mapped(p, len) != 1) fail("a fresh mapping not in maps");
        munmap(p, len);
        if (mapped(p, len) != 0) fail("an unmapped range still in maps");
    }
    return NULL;
}

static void *fd_thread(void *arg) {
    long id = (long)arg;
    char path[80], link[80], got[80];
    snprintf(path, sizeof path, "%s-%ld", tmpl, id);
    for (int i = 0; i < ROUNDS; i++) {
        int fd = open(path, O_RDWR | O_CREAT, 0600);
        if (fd < 0) { fail("open"); continue; }
        snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
        ssize_t n = readlink(link, got, sizeof got - 1);
        if (n < 0) fail("fd link missing");
        else {
            got[n] = 0;
            if (strcmp(got, path)) fail("fd link names another file");
        }
        close(fd);
    }
    unlink(path);
    return NULL;
}

static int cmdline_is(char **argv) {
    char buf[4096], want[4096];
    int n = slurp("/proc/self/cmdline", buf, sizeof buf), w = 0;
    for (char **a = argv; *a; a++) {
        int l = strlen(*a) + 1;
        memcpy(want + w, *a, l);
        w += l;
    }
    return n == w && !memcmp(buf, want, w);
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "after-exec")) return cmdline_is(argv) ? 0 : 3;
    snprintf(tmpl, sizeof tmpl, "/tmp/proc-threads-%d", (int)getpid());
    if (!cmdline_is(argv)) fail("cmdline");
    pthread_t t[2 + THREADS];
    for (long i = 0; i < 2; i++) pthread_create(&t[i], NULL, maps_thread, (void *)i);
    for (long i = 0; i < THREADS; i++) pthread_create(&t[2 + i], NULL, fd_thread, (void *)i);
    /* A forked child: its own pid in stat, its parent's argv in cmdline. */
    for (int i = 0; i < 4; i++) {
        pid_t c = fork();
        if (c == 0) {
            char buf[512];
            if (slurp("/proc/self/stat", buf, sizeof buf) <= 0 || atoi(buf) != getpid()) _exit(1);
            _exit(cmdline_is(argv) ? 0 : 2);
        }
        int st = 0;
        if (c < 0 || waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st)) fail("child's /proc/self");
    }
    for (int i = 0; i < 2 + THREADS; i++) pthread_join(t[i], NULL);
    /* The image after execve. */
    pid_t c = fork();
    if (c == 0) {
        char *av[] = {argv[0], "after-exec", "x y", NULL};
        execv(argv[0], av);
        _exit(4);
    }
    int st = 0;
    if (c < 0 || waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st)) fail("cmdline after exec");
    printf("rounds %d bad %ld\n", ROUNDS, bad);
    return bad != 0;
}
