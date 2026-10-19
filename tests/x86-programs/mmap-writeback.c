/* MAP_SHARED write-back by page (scratch/KC/map-shared.md, option B): a
 * forked child and its parent write different pages of one shared file
 * mapping, the child exits without munmap, the parent unmaps, and the file
 * keeps both pages (each process writes back only the pages it changed, and
 * an exit writes back as Linux's page cache keeps the stores). Then a
 * mapping a process exits without unmapping. Run in a scratch directory
 * (the cwd). Prints one line per check, then "mmap-writeback ok". */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;

static void check(const char *what, int ok) {
    printf("%s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}

int main(void) {
    char path[64], buf[4];
    snprintf(path, sizeof path, "mmap-writeback.%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || ftruncate(fd, 8192)) { perror("open"); return 2; }
    char *r = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (r == MAP_FAILED) { perror("mmap"); return 2; }
    pid_t child = fork();
    if (child == 0) {
        r[4096] = 'C';
        _exit(0);
    }
    r[0] = 'P';
    int st;
    waitpid(child, &st, 0);
    munmap(r, 8192);
    memset(buf, 0, sizeof buf);
    check("fork pages kept", pread(fd, buf, 1, 0) == 1 && buf[0] == 'P' && pread(fd, buf + 1, 1, 4096) == 1 && buf[1] == 'C');
    child = fork();
    if (child == 0) {
        char *q = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (q == MAP_FAILED) _exit(2);
        q[7] = 'X';
        _exit(0);
    }
    waitpid(child, &st, 0);
    memset(buf, 0, sizeof buf);
    check("exit writes back", WIFEXITED(st) && WEXITSTATUS(st) == 0 && pread(fd, buf, 1, 7) == 1 && buf[0] == 'X');
    close(fd);
    unlink(path);
    printf("mmap-writeback %s\n", fails ? "FAIL" : "ok");
    return fails != 0;
}
