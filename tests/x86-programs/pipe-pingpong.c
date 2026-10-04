/* Two processes bouncing a byte over a pair of pipes, 2000 times. Each
 * hop wakes a reader parked in read(): under gex that used to take a 1 ms
 * poll turn (4 s here); now the writer's syscall wakes it. The output is
 * only whether it took under a second, so it matches native. */
#include <stdio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    int ab[2], ba[2];
    if (pipe(ab) || pipe(ba)) return 1;
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        while (read(ab[0], &c, 1) == 1 && c) write(ba[1], &c, 1);
        _exit(0);
    }
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    char c = 1;
    for (int i = 0; i < 2000; i++) {
        write(ab[1], &c, 1);
        if (read(ba[0], &c, 1) != 1) return 2;
    }
    c = 0;
    write(ab[1], &c, 1);
    waitpid(pid, 0, 0);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    double s = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) * 1e-9;
    printf("2000 round trips under a second: %s\n", s < 1.0 ? "yes" : "no");
    return 0;
}
