#define _GNU_SOURCE
#include <sched.h>
#include <sys/wait.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static int lines(const char *path, const char *prefix, int skip_aggregate) {
    char b[512]; int n = 0; FILE *f = fopen(path, "r");
    if (!f) return -1;
    while (fgets(b, sizeof b, f))
        if (!strncmp(b, prefix, strlen(prefix)) && !(skip_aggregate && b[3] == ' ')) n++;
    fclose(f);
    return n;
}
int main(int argc, char **argv) {
    cpu_set_t s; CPU_ZERO(&s);
    int aff = sched_getaffinity(0, sizeof s, &s) == 0 ? CPU_COUNT(&s) : -1;
    /* /sys/devices/system/cpu: the online list, and one topology entry per processor. */
    int lo = -1, hi = -1, sys = -1, topo = 0;
    FILE *f = fopen("/sys/devices/system/cpu/online", "r");
    if (f) { int k = fscanf(f, "%d-%d", &lo, &hi); fclose(f); sys = k == 2 ? hi - lo + 1 : k == 1 ? 1 : -1; }
    for (int i = 0; i < 1024; i++) {
        char p[96]; snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu%d/topology/core_id", i);
        if (!(f = fopen(p, "r"))) break;
        fclose(f); topo++;
    }
    printf("%saffinity=%d sysconf=%ld stat=%d cpuinfo=%d sys=%d topology=%d\n", argc > 1 ? "child " : "", aff, sysconf(_SC_NPROCESSORS_ONLN),
           lines("/proc/stat", "cpu", 1), lines("/proc/cpuinfo", "processor", 0), sys, topo);
    /* A child, in a process of its own (a fresh Worker on the page), sees the same. */
    if (argc == 1) {
        int st;
        fflush(stdout);
        pid_t pid = fork();
        if (pid == 0) { char *args[] = { argv[0], "child", 0 }; int r = main(2, args); fflush(stdout); _exit(r); }
        if (pid < 0 || waitpid(pid, &st, 0) != pid) return 1;
    }
    return 0;
}
