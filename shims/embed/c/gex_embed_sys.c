// gex-embed: Linux system calls of an arm64 guest, done with the host's libc (emscripten's). Compiled by emcc into
// the host, beside libgex_embed.a, which calls gex_embed_sys for every call it doesn't handle itself (memory,
// threads, signals: src/sys.rs).
//
// Every function the Rust side calls has fixed-width types: `long` is 32 bits here, and a call whose wasm type
// disagrees with the definition's is linked as a trap.
//
// Guest file descriptors are the host's: an fd the guest opens is one the host's Java side can read, and the other
// way round. What changes on the way is the ABI: arm64 Linux's flag bits, struct layouts and errno numbers on the
// guest's side; musl-on-WASI's on the host's.
#define _GNU_SOURCE
#include <dirent.h>
#include <emscripten/threading.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

// emscripten's getdents64, which writes Linux's struct linux_dirent64 (emscripten's struct dirent is the same layout).
int __syscall_getdents64(int fd, intptr_t dirp, size_t count);

#define G(a) ((void*)(uintptr_t)(a))

// Linux's errno numbers, for the host's.
static long lx(int e) {
    switch (e) {
    case EPERM: return 1; case ENOENT: return 2; case ESRCH: return 3; case EINTR: return 4; case EIO: return 5;
    case ENXIO: return 6; case E2BIG: return 7; case ENOEXEC: return 8; case EBADF: return 9; case ECHILD: return 10;
    case EAGAIN: return 11; case ENOMEM: return 12; case EACCES: return 13; case EFAULT: return 14;
    case EBUSY: return 16; case EEXIST: return 17; case EXDEV: return 18; case ENODEV: return 19;
    case ENOTDIR: return 20; case EISDIR: return 21; case EINVAL: return 22; case ENFILE: return 23;
    case EMFILE: return 24; case ENOTTY: return 25; case ETXTBSY: return 26; case EFBIG: return 27;
    case ENOSPC: return 28; case ESPIPE: return 29; case EROFS: return 30; case EMLINK: return 31;
    case EPIPE: return 32; case EDOM: return 33; case ERANGE: return 34; case EDEADLK: return 35;
    case ENAMETOOLONG: return 36; case ENOLCK: return 37; case ENOSYS: return 38; case ENOTEMPTY: return 39;
    case ELOOP: return 40; case ENOTSOCK: return 88; case EOPNOTSUPP: return 95; case EAFNOSUPPORT: return 97;
    case EADDRINUSE: return 98; case ECONNREFUSED: return 111; case ETIMEDOUT: return 110;
    case ENETUNREACH: return 101; case ECONNRESET: return 104; case EINPROGRESS: return 115;
    case ENOTCONN: return 107; case EALREADY: return 114; case EOVERFLOW: return 75;
    default: return 5;
    }
}

static long ret(long r) { return r < 0 ? -lx(errno) : r; }

// arm64 Linux's open flags (the generic ones, but with O_DIRECTORY, O_NOFOLLOW, O_DIRECT and O_LARGEFILE moved).
static int open_flags(uint64_t f) {
    int o = f & 3;
    static const struct { uint32_t linux; int host; } map[] = {
        {0100, O_CREAT}, {0200, O_EXCL}, {0400, O_NOCTTY}, {01000, O_TRUNC}, {02000, O_APPEND},
        {04000, O_NONBLOCK}, {010000, O_DSYNC}, {040000, O_DIRECTORY}, {0100000, O_NOFOLLOW},
        {01000000, O_NOATIME}, {02000000, O_CLOEXEC}, {010000000, O_PATH},
    };
    for (unsigned i = 0; i < sizeof map / sizeof *map; i++)
        if (f & map[i].linux) o |= map[i].host;
    return o;
}

static uint64_t linux_open_flags(int h) {
    uint64_t f = h & 3;
    if (h & O_APPEND) f |= 02000;
    if (h & O_NONBLOCK) f |= 04000;
    if (h & O_DIRECTORY) f |= 040000;
    if (h & O_CLOEXEC) f |= 02000000;
    return f;
}

// arm64's struct stat (128 bytes).
static void put_stat(void* g, const struct stat* s) {
    uint8_t* p = g;
    memset(p, 0, 128);
    *(uint64_t*)(p + 0) = s->st_dev;
    *(uint64_t*)(p + 8) = s->st_ino;
    *(uint32_t*)(p + 16) = s->st_mode;
    *(uint32_t*)(p + 20) = s->st_nlink;
    *(uint32_t*)(p + 24) = s->st_uid;
    *(uint32_t*)(p + 28) = s->st_gid;
    *(uint64_t*)(p + 32) = s->st_rdev;
    *(int64_t*)(p + 48) = s->st_size;
    *(int32_t*)(p + 56) = s->st_blksize;
    *(int64_t*)(p + 64) = s->st_blocks;
    *(int64_t*)(p + 72) = s->st_atim.tv_sec;
    *(int64_t*)(p + 80) = s->st_atim.tv_nsec;
    *(int64_t*)(p + 88) = s->st_mtim.tv_sec;
    *(int64_t*)(p + 96) = s->st_mtim.tv_nsec;
    *(int64_t*)(p + 104) = s->st_ctim.tv_sec;
    *(int64_t*)(p + 112) = s->st_ctim.tv_nsec;
}

// arm64's struct statfs (120 bytes).
static void put_statfs(void* g, const struct statfs* s) {
    uint64_t* p = g;
    memset(p, 0, 120);
    p[0] = s->f_type;
    p[1] = s->f_bsize;
    p[2] = s->f_blocks;
    p[3] = s->f_bfree;
    p[4] = s->f_bavail;
    p[5] = s->f_files;
    p[6] = s->f_ffree;
    p[8] = s->f_namelen;
    p[9] = s->f_frsize;
}

// A guest timespec ({i64, i64}) as milliseconds, or INFINITY for none.
static double ms(const int64_t* ts) { return ts ? ts[0] * 1e3 + ts[1] / 1e6 : INFINITY; }

static double now_ms(int clk) {
    struct timespec t;
    clock_gettime(clk, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static long futex(uint64_t addr, int op, uint32_t val, const int64_t* ts) {
    int cmd = op & 127;
    int clk = (op & 256) ? CLOCK_REALTIME : CLOCK_MONOTONIC;
    volatile void* a = G(addr);
    switch (cmd) {
    case 0:   // FUTEX_WAIT: a relative timeout
    case 9: { // FUTEX_WAIT_BITSET: an absolute one
        double t = ms(ts);
        if (ts && cmd == 9) t = fmax(0, t - now_ms(clk));
        int r = emscripten_futex_wait(a, val, t);
        if (r == 0) return 0;
        return r == -ETIMEDOUT ? -110 : -11;
    }
    case 1:  // FUTEX_WAKE
    case 10: // FUTEX_WAKE_BITSET
        return emscripten_futex_wake(a, val > 0x7fffffff ? 0x7fffffff : val);
    default:
        return -38;
    }
}

static long uname_(void* g) {
    char* u = g;
    memset(u, 0, 6 * 65);
    strcpy(u + 0 * 65, "Linux");
    strcpy(u + 1 * 65, "localhost");
    strcpy(u + 2 * 65, "6.6.30-android15");
    strcpy(u + 3 * 65, "#1 SMP PREEMPT");
    strcpy(u + 4 * 65, "aarch64");
    strcpy(u + 5 * 65, "localdomain");
    return 0;
}

static long rlimit(int res, uint64_t* old) {
    if (!old) return 0;
    const uint64_t inf = ~0ull;
    switch (res) {
    case 3: old[0] = 8 << 20; old[1] = inf; break;  // RLIMIT_STACK
    case 7: old[0] = 32768; old[1] = 32768; break;  // RLIMIT_NOFILE
    default: old[0] = inf; old[1] = inf;
    }
    return 0;
}

static long sysinfo_(void* g) {
    uint8_t* p = g;
    memset(p, 0, 112);
    *(int64_t*)(p + 0) = (int64_t)(now_ms(CLOCK_MONOTONIC) / 1000);
    *(uint64_t*)(p + 32) = 4ull << 30;  // totalram
    *(uint64_t*)(p + 40) = 2ull << 30;  // freeram
    *(uint16_t*)(p + 80) = 1;           // procs
    *(uint32_t*)(p + 104) = 1;          // mem_unit
    return 0;
}

int64_t gex_embed_sys(int64_t nr, uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3, uint64_t a4, uint64_t a5) {
    (void)a5;
    switch (nr) {
    case 17: return getcwd(G(a0), a1) ? (long)strlen(G(a0)) + 1 : -lx(errno);
    case 23: return ret(dup(a0));
    case 24: return ret(dup3(a0, a1, a2 & 02000000 ? O_CLOEXEC : 0));
    case 25: // fcntl
        switch (a1) {
        case 0: case 1030: return ret(fcntl(a0, (int)a1, (int)a2));
        case 1: return ret(fcntl(a0, F_GETFD));
        case 2: return ret(fcntl(a0, F_SETFD, (int)a2));
        case 3: { long r = fcntl(a0, F_GETFL); return r < 0 ? -lx(errno) : (long)linux_open_flags(r); }
        case 4: return ret(fcntl(a0, F_SETFL, open_flags(a2)));
        default: return 0;  // locks: one process, nothing to lock against
        }
    case 29: return -25;  // ioctl: ENOTTY
    case 34: return ret(mkdirat(a0, G(a1), a2));
    case 35: return ret(unlinkat(a0, G(a1), a2));
    case 38: return ret(renameat(a0, G(a1), a2, G(a3)));
    case 43: { struct statfs s; long r = statfs(G(a0), &s); if (r == 0) put_statfs(G(a1), &s); return ret(r); }
    case 44: { struct statfs s; long r = fstatfs(a0, &s); if (r == 0) put_statfs(G(a1), &s); return ret(r); }
    case 46: return ret(ftruncate(a0, a1));
    case 48: return ret(faccessat(a0, G(a1), a2, 0));
    case 49: return ret(chdir(G(a0)));
    case 56: return ret(openat(a0, G(a1), open_flags(a2), (mode_t)a3));
    case 57: return ret(close(a0));
    case 59: { int fds[2]; long r = pipe2(fds, a1 & 02000000 ? O_CLOEXEC : 0);
               if (r == 0) memcpy(G(a0), fds, sizeof fds); return ret(r); }
    case 61: { long r = __syscall_getdents64(a0, (intptr_t)G(a1), a2); return r < 0 ? -lx(-r) : r; }
    case 62: return ret(lseek(a0, (off_t)a1, a2));
    case 63: return ret(read(a0, G(a1), a2));
    case 64: return ret(write(a0, G(a1), a2));
    case 65:
    case 66: { // readv, writev: arm64's iovec is {u64, u64}
        const uint64_t* iov = G(a1);
        long total = 0;
        for (uint64_t i = 0; i < a2; i++) {
            long r = nr == 65 ? read(a0, G(iov[2 * i]), iov[2 * i + 1]) : write(a0, G(iov[2 * i]), iov[2 * i + 1]);
            if (r < 0) return total ? total : -lx(errno);
            total += r;
            if ((uint64_t)r < iov[2 * i + 1]) break;
        }
        return total;
    }
    case 67: return ret(pread(a0, G(a1), a2, (off_t)a3));
    case 68: return ret(pwrite(a0, G(a1), a2, (off_t)a3));
    case 78: return ret(readlinkat(a0, G(a1), G(a2), a3));
    case 79: { struct stat s; long r = fstatat(a0, G(a1), &s, a3); if (r == 0) put_stat(G(a2), &s); return ret(r); }
    case 80: { struct stat s; long r = fstat(a0, &s); if (r == 0) put_stat(G(a1), &s); return ret(r); }
    case 82: case 83: return 0;
    case 98: return futex(a0, a1, a2, G(a3));
    case 99: return 0;  // set_robust_list
    case 101: { // nanosleep
        const int64_t* ts = G(a0);
        struct timespec t = {ts[0], ts[1]};
        return ret(nanosleep(&t, 0));
    }
    case 113: case 114: { // clock_gettime, clock_getres
        struct timespec t;
        int clk = a0 == 1 || a0 == 4 || a0 == 6 || a0 == 7 ? CLOCK_MONOTONIC : CLOCK_REALTIME;
        long r = nr == 113 ? clock_gettime(clk, &t) : clock_getres(clk, &t);
        if (r == 0 && a1) { int64_t* g = G(a1); g[0] = t.tv_sec; g[1] = t.tv_nsec; }
        return ret(r);
    }
    case 115: { // clock_nanosleep
        const int64_t* ts = G(a2);
        int clk = a0 == 0 ? CLOCK_REALTIME : CLOCK_MONOTONIC;
        double wait = ms(ts) - (a1 & 1 ? now_ms(clk) : 0);
        if (wait > 0) {
            struct timespec t = {(time_t)(wait / 1e3), (long)(fmod(wait, 1e3) * 1e6)};
            nanosleep(&t, 0);
        }
        return 0;
    }
    case 123: { // sched_getaffinity: one CPU
        if (a1 < 8) return -22;
        memset(G(a2), 0, a1);
        *(uint8_t*)G(a2) = 1;
        return 8;
    }
    case 124: sched_yield(); return 0;
    case 119: return 0;  // sched_setscheduler: one policy
    case 120: return 0;  // sched_getscheduler: SCHED_NORMAL
    case 121: *(int32_t*)G(a1) = 0; return 0;  // sched_getparam
    case 160: return uname_(G(a0));
    case 163: return rlimit(a0, G(a1));
    case 261: return rlimit(a1, G(a3));
    case 169: { // gettimeofday
        struct timespec t;
        clock_gettime(CLOCK_REALTIME, &t);
        if (a0) { int64_t* g = G(a0); g[0] = t.tv_sec; g[1] = t.tv_nsec / 1000; }
        return 0;
    }
    case 173: return 1;                          // getppid
    case 174: case 175: case 176: case 177: return 10000;  // the app's uid and gid
    case 179: return sysinfo_(G(a0));
    case 198: return -13;  // socket: EACCES, as an app without the INTERNET permission
    case 278: { // getrandom
        uint8_t* p = G(a0);
        for (uint64_t done = 0; done < a1;) {
            size_t n = a1 - done > 256 ? 256 : a1 - done;
            if (getentropy(p + done, n)) return done ? (long)done : -lx(errno);
            done += n;
        }
        return a1;
    }
    default: return -38;  // ENOSYS
    }
}

// Read up to `n` bytes at `off` into the guest's `buf` (a file mapping), as Linux's errno on failure.
int64_t gex_embed_pread(int fd, uint64_t buf, uint64_t n, uint64_t off) {
    uint64_t done = 0;
    while (done < n) {
        long r = pread(fd, G(buf + done), n - done, (off_t)(off + done));
        if (r < 0) return -lx(errno);
        if (r == 0) break;
        done += r;
    }
    return done;
}
