/* A child that outlives the moment it was started.
 *
 * `fork` then immediately `wait` is the shape every shell has, and it is the
 * shape that worked in a browser tab for a long time while nothing else did.
 * The browser host has one address space and no way to make a second, so a
 * fork was a checkpoint: the parent was copied aside and the child carried on
 * in its place, and the parent came back when the child *exited*.
 *
 * Which is fine until the child does not exit. `Xvfb &` is a daemon; so is
 * every helper a program starts and then talks to. Under a checkpoint the
 * shell after an `&` simply never ran again.
 *
 * So this starts a child that stays alive, keeps working in the parent while
 * it does, sends it a message it can only receive by being scheduled, and
 * collects it at the end.
 *
 * Every line has to be ordered by an event -- a pipe read, a write, an exit
 * status -- and not by which process happens to be running. Two processes
 * that both print before either waits produce whichever order the scheduler
 * chose, and the first version of this file did exactly that and passed or
 * failed depending on the run.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void put(const char *s) {
    /* Unbuffered, because the interleaving between two processes is the point
     * and stdio would reorder it into whichever flushed last. */
    write(1, s, strlen(s));
}

int main(void) {
    int down[2], up[2];
    if (pipe(down) != 0 || pipe(up) != 0) {
        put("pipe failed\n");
        return 1;
    }

    pid_t kid = fork();
    if (kid < 0) {
        put("fork failed\n");
        return 1;
    }
    if (kid == 0) {
        close(down[1]);
        close(up[0]);
        /* Nothing is printed before the read. The parent has to run to send
         * anything, so reaching the end of this read at all is the whole
         * test -- and a line printed *before* it would be ordered by nothing
         * but scheduling, which is how the first version of this file came
         * out one way under QEMU and the other in a page. */
        char buf[32];
        ssize_t n = read(down[0], buf, sizeof buf - 1);
        if (n <= 0) {
            put("child: the parent never wrote\n");
            _exit(2);
        }
        buf[n] = 0;
        put("child: got ");
        put(buf);
        put("\n");
        write(up[1], "ok", 2);
        _exit(7);
    }

    close(down[0]);
    close(up[1]);
    /* The parent keeps going while the child is alive -- the line that never
     * appeared under a checkpoint. */
    put("parent: still running\n");
    long acc = 0;
    for (long i = 0; i < 200000; i++) {
        acc += i % 7;
    }
    if (acc == 0) {
        put("parent: impossible\n");
    }
    put("parent: sending work\n");
    write(down[1], "hello", 5);

    char reply[8];
    ssize_t n = read(up[0], reply, sizeof reply - 1);
    reply[n > 0 ? n : 0] = 0;
    put("parent: child replied ");
    put(reply);
    put("\n");

    int status = 0;
    if (waitpid(kid, &status, 0) != kid) {
        put("parent: waitpid did not find the child\n");
        return 1;
    }
    printf("parent: child exited %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return 0;
}
