/* A syscall that parks while another process runs.
 *
 * The browser host has one wasm stack and one address space, so a process that
 * blocks is written out to a snapshot and another is written in. When the
 * first one is put back, the syscall it parked in still owes it a return
 * value, and only the scheduler can supply it.
 *
 * It did not always get the chance. `install_process` primed the resume point
 * when it swapped a process in, on the reasoning that the scheduler runs next
 * and overwrites it. It does -- unless it finds nothing ready and gives the
 * stack back to the page, which does not overwrite anything. The page then
 * read that primed value and re-entered the guest at the instruction after its
 * `svc`, with no return value ever written.
 *
 * What that looks like from in here is a syscall returning its own first
 * argument. `epoll_wait` came back with 5, which was the epoll descriptor
 * still sitting in x0, and the caller walked five events off the end of an
 * array that had none in it. In an X server the same thing landed as a
 * link register full of rubbish and a branch into unmapped memory.
 *
 * So: park in a syscall that can only be finished by the host, make sure
 * another process runs while it is parked, and check what the syscall
 * returned. A timeout is the wake rather than a descriptor becoming ready,
 * because that is the path that goes out to the page and back.
 */
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <unistd.h>

/* Written rather than printed: the child ends with `_exit`, which does not
 * flush, and the interesting line is the last one. */
static void put(const char *s) { write(1, s, strlen(s)); }

int main(void) {
    int p[2];
    if (pipe(p) != 0) {
        puts("pipe failed");
        return 1;
    }

    pid_t kid = fork();
    if (kid == 0) {
        /* Both ends stay open, so the pipe never reports end-of-file and
         * nothing is ever ready: the wake has to be the timeout. */
        int ep = epoll_create1(0);
        struct epoll_event ev = {.events = EPOLLIN, .data.fd = p[0]};
        epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
        /* 256, as an X server asks for, so a miscounted return walks a long
         * way past anything real. */
        struct epoll_event out[256];
        memset(out, 0, sizeof out);
        int n = epoll_wait(ep, out, 256, 1500);
        char msg[64];
        snprintf(msg, sizeof msg, "child: epoll_wait returned %d\n", n);
        put(msg);
        /* Reached only if the return above did not send the caller somewhere
         * else entirely. */
        snprintf(msg, sizeof msg, "child: first event %#x\n", out[0].events);
        put(msg);
        _exit(n == 0 ? 0 : 1);
    }

    /* And then the parent *leaves*, rather than waiting. That is what makes
     * the host swap the child back in with nothing else to run -- the moment
     * the defect lived in. A parent that waits keeps a runnable thread
     * around, the host never idles, and the bug does not appear at all: the
     * first version of this test did that and passed against the broken
     * runtime.
     *
     * It also means this is a browser-runtime test and not a comparison. The
     * native adapter ends the whole run when its first process exits, so the
     * child's lines never appear there.
     */
    (void)kid;
    put("parent: leaving\n");
    return 0;
}
