/* A memfd across fork: the child has the parent's descriptor, reads what the
 * parent wrote, and what it writes through a shared mapping is the parent's
 * to read. (A toolkit's cursor theme is a memfd its forked children carry.) */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int fd = memfd_create("memfdfork", MFD_CLOEXEC);
    if (fd < 0) { perror("memfd_create"); return 1; }
    if (ftruncate(fd, 4096) != 0 || pwrite(fd, "parent", 6, 0) != 6) { perror("write"); return 1; }
    char *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) { perror("mmap"); return 1; }
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 1; }
    if (pid == 0) {
        struct stat st;
        char buf[7] = {0};
        int ok = fstat(fd, &st) == 0;
        printf("child fstat: %s, size %lld\n", ok ? "ok" : "failed", ok ? (long long)st.st_size : -1LL);
        printf("child read: %s\n", pread(fd, buf, 6, 0) == 6 ? buf : "failed");
        printf("child mapping: %.6s\n", map);
        memcpy(map + 100, "child", 5);
        pwrite(fd, "fd", 2, 200);
        fflush(stdout);
        _exit(0);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    char buf[3] = {0};
    pread(fd, buf, 2, 200);
    printf("parent sees the child's mapping write: %.5s\n", map + 100);
    printf("parent sees the child's fd write: %s\n", buf);
    printf("child exit %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return 0;
}
