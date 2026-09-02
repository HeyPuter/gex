// xtrace: single-step a native x86-64 process under ptrace and print one line
// per instruction with the registers, in the same format gex's
// GEX_TRACE_BLOCKS emits, so the two can be aligned and diffed
// (tools/xdiff.py). ASLR is disabled so addresses are stable.
//
//   cc -O2 -o xtrace xtrace.c
//   ./xtrace [-n MAXSTEPS] prog args... > native.txt
//
// Each line: pc=0x.. rax=0x.. rcx=.. rdx rbx rsp rbp rsi rdi r8..r15 fl=0x..
// followed by "maps" lines at start describing the loaded objects, so the
// diff tool can translate addresses between the two address spaces.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <sys/user.h>
#include <sys/personality.h>
#include <sys/types.h>

static void dump_maps(pid_t pid) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/maps", pid);
    FILE *f = fopen(path, "r");
    if (!f) return;
    char line[512];
    while (fgets(line, sizeof line, f)) {
        printf("map %s", line);
    }
    fclose(f);
}

int main(int argc, char **argv) {
    long max_steps = 50000000;
    int ai = 1;
    if (argc > 2 && strcmp(argv[1], "-n") == 0) {
        max_steps = atol(argv[2]);
        ai = 3;
    }
    if (ai >= argc) {
        fprintf(stderr, "usage: xtrace [-n MAX] prog args...\n");
        return 2;
    }
    pid_t pid = fork();
    if (pid == 0) {
        personality(ADDR_NO_RANDOMIZE);
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        execv(argv[ai], argv + ai);
        perror("execv");
        _exit(127);
    }
    int status;
    waitpid(pid, &status, 0);
    if (!WIFSTOPPED(status)) {
        fprintf(stderr, "child did not stop\n");
        return 1;
    }
    // Maps are read at the first stop (after exec): the interpreter and the
    // executable are mapped, libraries are not yet.
    dump_maps(pid);
    setvbuf(stdout, NULL, _IOFBF, 1 << 20);
    struct user_regs_struct r;
    long steps = 0;
    for (;;) {
        if (ptrace(PTRACE_GETREGS, pid, 0, &r) < 0) break;
        printf("pc=%#llx rax=%#llx rcx=%#llx rdx=%#llx rbx=%#llx rsp=%#llx rbp=%#llx rsi=%#llx rdi=%#llx "
               "r8=%#llx r9=%#llx r10=%#llx r11=%#llx r12=%#llx r13=%#llx r14=%#llx r15=%#llx fl=%#llx\n",
               r.rip, r.rax, r.rcx, r.rdx, r.rbx, r.rsp, r.rbp, r.rsi, r.rdi, r.r8, r.r9, r.r10, r.r11,
               r.r12, r.r13, r.r14, r.r15, r.eflags);
        if (++steps >= max_steps) {
            fprintf(stderr, "xtrace: step limit reached\n");
            kill(pid, 9);
            break;
        }
        if (ptrace(PTRACE_SINGLESTEP, pid, 0, 0) < 0) break;
        if (waitpid(pid, &status, 0) < 0) break;
        if (WIFEXITED(status) || WIFSIGNALED(status)) break;
        if (WIFSTOPPED(status) && WSTOPSIG(status) != SIGTRAP) {
            // Deliver other signals to the child and keep stepping.
            fprintf(stderr, "xtrace: signal %d\n", WSTOPSIG(status));
            ptrace(PTRACE_SINGLESTEP, pid, 0, WSTOPSIG(status));
            waitpid(pid, &status, 0);
            if (WIFEXITED(status) || WIFSIGNALED(status)) break;
        }
    }
    fflush(stdout);
    fprintf(stderr, "xtrace: %ld steps\n", steps);
    return 0;
}
