/* Advisory record locks through fcntl: F_SETLK, F_SETLKW and F_GETLK (an
 * i386 musl program issues them as fcntl64's F_*LK64, 12-14). wineserver
 * takes one on its lock file at start-up. With no other process holding a
 * lock, F_GETLK answers F_UNLCK. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
    char p[] = "/tmp/gex-fcntl-locks-XXXXXX";
    int fd = mkstemp(p);
    if (fd < 0) return 1;
    struct flock fl = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1 };
    printf("F_SETLK = %d\n", fcntl(fd, F_SETLK, &fl));
    printf("F_SETLKW = %d\n", fcntl(fd, F_SETLKW, &fl));
    struct flock q = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1 };
    printf("F_GETLK = %d, l_type F_UNLCK %d\n", fcntl(fd, F_GETLK, &q), q.l_type == F_UNLCK);
    fl.l_type = F_UNLCK;
    printf("unlock = %d\n", fcntl(fd, F_SETLK, &fl));
    printf("F_SETLK on a bad fd = %d\n", fcntl(99, F_SETLK, &fl));
    close(fd);
    unlink(p);
    return 0;
}
