/* gldev: the guest of test/page/gldev.test.js. */
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
static void probe(const char *who) {
    struct stat st, fst;
    int s = stat("/dev/gex-gl", &st);
    int a = access("/dev/gex-gl", R_OK | W_OK);
    int fd = open("/dev/gex-gl", O_RDWR);
    int f = fd >= 0 ? fstat(fd, &fst) : -1;
    printf("%s: stat %s, access %s, open %s, fstat %s\n", who,
           s == 0 ? (S_ISCHR(st.st_mode) ? "chr" : "other") : "no",
           a == 0 ? "ok" : "no", fd >= 0 ? "ok" : "no",
           f == 0 ? (S_ISCHR(fst.st_mode) ? "chr" : "other") : "no");
    if (fd >= 0) close(fd);
    fflush(stdout);
}
int main(void) {
    probe("parent");
    pid_t p = fork();
    if (p == 0) { probe("child"); _exit(0); }
    int st;
    waitpid(p, &st, 0);
    return 0;
}
