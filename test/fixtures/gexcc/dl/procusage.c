// /proc's processor time and memory are measured, not made up: a process
// that computes for 300 ms and sleeps for 300 ms has used about 300 ms
// (its stat's utime, CLOCK_PROCESS_CPUTIME_ID), the system's busy time
// (/proc/stat) grows by its own and its child's, a collected child's time
// is the parent's cutime, /proc/uptime advances, statm's resident size is
// not zero, and sysinfo agrees. getrusage(RUSAGE_CHILDREN) and times'
// cutime count the child too, and an unknown who is EINVAL. btop showed 0% everywhere and a frozen uptime without it.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/resource.h>
#include <sys/sysinfo.h>
#include <sys/times.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static double mono(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static double cputime(void) { struct timespec t; clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static volatile unsigned long sink;
// Spun by the process's own processor time, not the clock: on a loaded
// host (a gate's parallel lanes) a spin by the clock got a quarter of a
// core and measured 0.08 s.
static void spin(double s) { double end = cputime() + s; while (cputime() < end) for (int i = 0; i < 10000; i++) sink += i; }
static void slurp(const char *p, char *buf, int n) { FILE *f = fopen(p, "r"); int k = f ? fread(buf, 1, n - 1, f) : 0; buf[k] = 0; if (f) fclose(f); }
// field i (1-based) of a stat line, counted after the ')'
static long field(const char *stat, int i) {
    const char *p = strrchr(stat, ')'); if (!p) return -1; p += 2;
    for (int f = 3; f < i; f++) { p = strchr(p, ' '); if (!p) return -1; p++; }
    return atol(p);
}
static long busy(void) { char b[4096]; slurp("/proc/stat", b, sizeof b); long u = -1; sscanf(b, "cpu %ld", &u); return u; }
static double uptime(void) { char b[128]; slurp("/proc/uptime", b, sizeof b); return atof(b); }
int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    char b[4096];
    double up0 = uptime(); long busy0 = busy(); double c0 = cputime();
    spin(0.3);
    usleep(300000);
    double c = cputime() - c0;
    slurp("/proc/self/stat", b, sizeof b);
    long utime = field(b, 14);
    // utime is the process's time from its start, its start-up's too (a
    // loaded host's longer one): bounded by the clock read beside it.
    double call = cputime();
    printf("cputime %s (%.2f s)\n", c > 0.2 && c < 0.5 ? "ok" : "BAD", c);
    printf("utime %s (%ld ticks, clock %.2f s)\n", utime >= 20 && utime <= call * 100 + 5 ? "ok" : "BAD", utime, call);
    // The child says what its clock read at its end, which its parent's
    // cutime then counts.
    int fd[2]; if (pipe(fd)) return 1;
    pid_t p = fork();
    if (p == 0) { spin(0.2); double t = cputime(); write(fd[1], &t, sizeof t); _exit(0); }
    double ct = 0; read(fd[0], &ct, sizeof ct);
    int st; waitpid(p, &st, 0);
    slurp("/proc/self/stat", b, sizeof b);
    long cutime = field(b, 16);
    printf("cutime %s (%ld ticks, child's clock %.2f s)\n", cutime >= 15 && cutime <= ct * 100 + 5 ? "ok" : "BAD", cutime, ct);
    struct rusage ru; getrusage(RUSAGE_CHILDREN, &ru);
    double rc = ru.ru_utime.tv_sec + ru.ru_utime.tv_usec / 1e6;
    struct tms tm; times(&tm);
    int bad = getrusage(99, &ru) == -1 && errno == EINVAL;
    printf("rusage children %s (%.2f s, times cutime %ld, who 99 %s)\n",
           rc >= 0.15 && rc <= ct + 0.05 && tm.tms_cutime >= 15 && bad ? "ok" : "BAD", rc, (long)tm.tms_cutime, bad ? "EINVAL" : "accepted");
    long db = busy() - busy0;
    printf("system busy %s (%ld ticks)\n", db >= 45 ? "ok" : "BAD", db);
    double du = uptime() - up0;
    printf("uptime %s (%.2f s)\n", du >= 0.75 ? "ok" : "BAD", du);
    slurp("/proc/self/statm", b, sizeof b);
    long size = 0, res = 0; sscanf(b, "%ld %ld", &size, &res);
    printf("statm %s\n", res > 0 && size >= res ? "ok" : "BAD");
    slurp("/proc/loadavg", b, sizeof b);
    double l1 = -1; sscanf(b, "%lf", &l1);
    printf("loadavg %s\n", l1 >= 0 ? "ok" : "BAD");
    struct sysinfo si; sysinfo(&si);
    double up = uptime();
    printf("sysinfo %s\n", si.uptime + 2 > up && si.uptime < up + 2 && si.freeram < si.totalram && si.procs >= 1 ? "ok" : "BAD");
    return 0;
}
