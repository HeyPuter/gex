/* Part of tools/x86-difftrace (run.sh). */
/* steptrace LO HI OUT PROG [ARGS]: single-step a 32-bit program under ptrace
 * and write the state before every instruction in [LO, HI), in the format of
 * gex's GEX_TRACE_INSNS log. */
#include <stdio.h>
#include <stdlib.h>
#include <sys/ptrace.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
int main(int argc, char **argv) {
    unsigned long lo = strtoul(argv[1], 0, 16), hi = strtoul(argv[2], 0, 16);
    FILE *out = fopen(argv[3], "w");
    pid_t p = fork();
    if (!p) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        int fd = open("/dev/null", 1); dup2(fd, 1);
        execv(argv[4], argv + 4);
        _exit(127);
    }
    int st;
    waitpid(p, &st, 0);
    unsigned long n = 0;
    for (;;) {
        struct user_regs_struct r;
        ptrace(PTRACE_GETREGS, p, 0, &r);
        unsigned long pc = r.rip & 0xffffffff;
        if (pc >= lo && pc < hi) {
            fprintf(out, "%lx %llx %llx %llx %llx %llx %llx %llx %llx %llx\n", pc,
                    r.rax & 0xffffffff, r.rcx & 0xffffffff, r.rdx & 0xffffffff, r.rbx & 0xffffffff,
                    r.rsp & 0xffffffff, r.rbp & 0xffffffff, r.rsi & 0xffffffff, r.rdi & 0xffffffff, r.eflags & 0xcd5);
            n++;
        }
        if (ptrace(PTRACE_SINGLESTEP, p, 0, 0) < 0) break;
        if (waitpid(p, &st, 0) < 0 || WIFEXITED(st) || WIFSIGNALED(st)) break;
        if (WIFSTOPPED(st) && WSTOPSIG(st) != SIGTRAP) {
            /* Deliver the program's own signals (its handlers report). */
            if (ptrace(PTRACE_SINGLESTEP, p, 0, WSTOPSIG(st)) < 0) break;
            if (waitpid(p, &st, 0) < 0 || WIFEXITED(st) || WIFSIGNALED(st)) break;
        }
    }
    fclose(out);
    fprintf(stderr, "%lu lines\n", n);
    return 0;
}
