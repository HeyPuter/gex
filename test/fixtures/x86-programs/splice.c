/* splice(2) between a pipe and a file both ways, and from a pipe whose
 * writer is another process (as `cat file | grep x` does in GNU grep).
 * Output: what arrived, and the counts. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int p[2];
    char path[] = "/tmp/gex-splice-XXXXXX", buf[64] = {0};
    int f = mkstemp(path);
    if (f < 0 || pipe(p)) return 1;
    unlink(path);
    write(p[1], "hello splice\n", 13);
    ssize_t a = splice(p[0], NULL, f, NULL, 64, 0);
    loff_t off = 0;
    ssize_t b = pread(f, buf, sizeof buf - 1, 0);
    printf("pipe->file %zd, read back %zd: %s", a, b, buf);
    ssize_t c = splice(f, &off, p[1], NULL, 5, 0);
    memset(buf, 0, sizeof buf);
    read(p[0], buf, sizeof buf - 1);
    printf("file->pipe %zd, offset now %lld: %s\n", c, (long long)off, buf);
    int q[2];
    if (pipe(q)) return 1;
    if (fork() == 0) {
        close(q[0]);
        for (int i = 0; i < 100; i++) write(q[1], "line of text\n", 13);
        _exit(0);
    }
    close(q[1]);
    int out = open("/dev/null", O_WRONLY);
    long total = 0;
    ssize_t n;
    while ((n = splice(q[0], NULL, out, NULL, 4096, 0)) > 0) total += n;
    wait(NULL);
    printf("from a writer process: %ld bytes, last %zd\n", total, n);
    return 0;
}
