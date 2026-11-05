/* A vfork child that changes the memory map it shares with its parent:
 * clone(CLONE_VM | CLONE_VFORK) whose child calls mmap. On Linux the call
 * succeeds and the child exits 3. gex runs such a child in its parent's
 * Worker (hosts/browser/src/vfork.rs) and cannot give it a mapping of its own
 * there, so it ends the child as a crash and says which call did it;
 * test/native/vfork-mmap checks both. */
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static char stack[64 * 1024];

static int child(void *arg) {
    (void)arg;
    void *p = mmap(0, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    _exit(p == MAP_FAILED ? 4 : 3);
}

int main(void) {
    int pid = clone(child, stack + sizeof stack, CLONE_VM | CLONE_VFORK | SIGCHLD, 0);
    if (pid < 0) {
        perror("clone");
        return 1;
    }
    int st = 0;
    while (waitpid(pid, &st, 0) < 0) {}
    if (WIFEXITED(st)) printf("child exited %d\n", WEXITSTATUS(st));
    else if (WIFSIGNALED(st)) printf("child killed by a signal %d\n", WTERMSIG(st));
    printf("parent goes on\n");
    return 0;
}
