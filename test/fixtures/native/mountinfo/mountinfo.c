/* What /proc/self/mountinfo and /proc/mounts say, against statx's mount
 * ids: the root and /proc each listed once, in Linux's fields, and each
 * one's id the one statx gives a path in it. */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/syscall.h>

static uint64_t mnt_id(const char *path)
{
    unsigned char sx[256] = {0};
    if (syscall(SYS_statx, AT_FDCWD, path, 0, 0x1000, sx) != 0)
        return 0;
    uint32_t mask;
    uint64_t id;
    memcpy(&mask, sx, 4);
    memcpy(&id, sx + 144, 8);
    return mask & 0x1000 ? id : 0;
}

int main(void)
{
    char line[1024];
    unsigned long root = 0, proc = 0, proc_parent = 0;
    int lines = 0;
    FILE *f = fopen("/proc/self/mountinfo", "r");
    if (!f) {
        puts("no mountinfo");
        return 1;
    }
    while (fgets(line, sizeof line, f)) {
        unsigned long id, parent;
        char at[512], type[64];
        if (sscanf(line, "%lu %lu %*s %*s %511s %*s - %63s", &id, &parent, at, type) != 4) {
            printf("bad line: %s", line);
            return 1;
        }
        lines++;
        if (!strcmp(at, "/"))
            root = id;
        if (!strcmp(at, "/proc") && !strcmp(type, "proc"))
            proc = id, proc_parent = parent;
    }
    fclose(f);
    printf("mountinfo: root %s, proc %s, proc under root %s\n", root ? "listed" : "missing",
           proc ? "listed" : "missing", proc_parent == root ? "yes" : "no");
    printf("statx: root %s, proc %s, a file in proc %s\n",
           mnt_id("/") == root ? "matches" : "differs",
           mnt_id("/proc") == proc ? "matches" : "differs",
           mnt_id("/proc/self/stat") == proc ? "matches" : "differs");
    int found = 0;
    f = fopen("/proc/mounts", "r");
    while (f && fgets(line, sizeof line, f))
        found |= !strncmp(line, "proc /proc proc ", 16);
    if (f)
        fclose(f);
    printf("mounts: proc %s\n", found ? "listed" : "missing");
    return 0;
}
