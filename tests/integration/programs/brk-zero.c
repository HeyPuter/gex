/* Heap the break has just moved over must read as zero.
 *
 * Linux guarantees it, and libc does not merely rely on that, it *skips work*
 * because of it: glibc's `calloc` omits its memset for the part of a chunk
 * that came from a fresh `sbrk`, since the kernel has already done it. A
 * `brk` that hands back whatever was last written there makes `calloc` return
 * uninitialised memory, and what notices first is something like the dynamic
 * loader finding a garbage field in a structure it just allocated.
 *
 * gex's `brk` moved the break and zeroed nothing. Growing into never-touched
 * memory hid that, because a fresh wasm memory is zero to begin with -- and a
 * new program gets a new one, so even an `execve` did not expose it. Lowering
 * the break and raising it again does: those bytes have been written, and
 * Linux clears them on the way back up.
 */
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define GROW (1024 * 1024)

/* `brk` directly, because musl's `sbrk` refuses any non-zero increment -- its
 * allocator uses `mmap` and it never implemented growing the break. */
static char *break_now(void) {
    return (char *)syscall(SYS_brk, 0);
}

/* Linux returns the new break on success and the old one on failure. */
static int move_break(char *to) {
    return (char *)syscall(SYS_brk, (unsigned long)to) == to;
}

int main(void) {
    char *base = break_now();

    if (!move_break(base + GROW)) {
        printf("could not grow the heap\n");
        return 1;
    }
    /* Something unmistakable, so stale memory cannot be mistaken for a
     * plausible-looking zero. */
    memset(base, 0xa5, GROW);

    if (!move_break(base)) {
        printf("could not lower the break\n");
        return 1;
    }
    if (!move_break(base + GROW)) {
        printf("could not regrow the heap\n");
        return 1;
    }

    long dirty = -1;
    for (long i = 0; i < GROW; i++) {
        if (base[i] != 0) {
            dirty = i;
            break;
        }
    }
    if (dirty < 0) {
        printf("regrown heap clean\n");
    } else {
        printf("regrown heap dirty at %ld (0x%02x)\n", dirty,
               (unsigned char)base[dirty]);
    }
    return 0;
}
