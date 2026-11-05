/* sendfile and copy_file_range: in-kernel descriptor-to-descriptor copies that
 * static file servers and node's fs fast paths use. Exercises the no-offset
 * form (advances the input position), the offset-pointer form (leaves it, bumps
 * the pointer), and copy_file_range with both offsets. Deterministic -> QEMU
 * is the oracle. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/syscall.h>
#include <unistd.h>

static const char *rd(const char *p) {
    static char b[128];
    int f = open(p, O_RDONLY);
    if (f < 0) return "<none>";
    int n = read(f, b, 127);
    close(f);
    if (n < 0) n = 0;
    b[n] = 0;
    return b;
}

int main(void) {
    chdir("/tmp");
    unlink("src"); unlink("dst"); unlink("dst2"); unlink("dst3");
    int s = open("src", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    write(s, "Hello, sendfile world!", 22);
    close(s);

    int in = open("src", O_RDONLY);
    int out = open("dst", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ssize_t r = sendfile(out, in, NULL, 22);
    close(in);
    close(out);
    printf("sendfile-noff ret=%zd dst=%s\n", r, rd("dst"));

    in = open("src", O_RDONLY);
    out = open("dst2", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    off_t off = 7;
    r = sendfile(out, in, &off, 15);
    close(in);
    close(out);
    printf("sendfile-off ret=%zd off=%ld dst2=%s\n", r, (long)off, rd("dst2"));

    in = open("src", O_RDONLY);
    out = open("dst3", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    loff_t io = 0, oo = 0;
    r = syscall(SYS_copy_file_range, in, &io, out, &oo, 5, 0);
    close(in);
    close(out);
    printf("cfr ret=%zd io=%ld oo=%ld dst3=%s\n", r, (long)io, (long)oo, rd("dst3"));

    unlink("src"); unlink("dst"); unlink("dst2"); unlink("dst3");
    printf("sendfile ok\n");
    return 0;
}
