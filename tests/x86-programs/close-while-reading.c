/* Descriptors read and written by some threads while others close them,
 * dup2 over them and reopen them; pipes and sockets under several readers
 * and writers at once. Reads and writes no longer take the kernel lock
 * (docs/kernel-locking.md step 3): a lookup reads the published descriptor
 * table, and a close publishes a new one.
 *
 *   files:   readers pread a descriptor another thread keeps closing and
 *            dup2-ing two different files onto; every read is all of one
 *            file or EBADF, never a mix, a crash or another errno.
 *   offsets: four threads read one descriptor of a 64 KiB file in 16-byte
 *            pieces; every piece is read exactly once (the offset's
 *            update is one step with the read, as Linux's f_pos_lock).
 *   eof:     a reader blocked on a pipe sees end of file when another
 *            thread closes the last two write ends.
 *   pipes:   four writers (write and writev) and two readers (read and
 *            readv) on one pipe; every writer's every byte arrives once.
 *   records: the same with fixed 64-byte records: a write of at most
 *            PIPE_BUF, vectored or not, is never split by another's.
 *   sockets: a stream socketpair carrying a byte sequence each way at once,
 *            written with write, writev and sendmsg, read with read, readv
 *            and recvmsg; every byte arrives in order.
 *
 * The output is one line per part, the same natively. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static void nap_ms(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, 0);
}

/* -- files ---------------------------------------------------------------- */

#define TARGET 100
static atomic_int stop;
static atomic_long reads_ok, reads_badf, bad;

static void *file_reader(void *arg) {
    (void)arg;
    char buf[64];
    while (!atomic_load(&stop)) {
        ssize_t r = pread(TARGET, buf, sizeof buf, 0);
        if (r > 0) {
            int mixed = 0;
            for (ssize_t i = 1; i < r; i++)
                if (buf[i] != buf[0]) mixed = 1;
            if (mixed || (buf[0] != 'A' && buf[0] != 'B'))
                atomic_fetch_add(&bad, 1);
            else
                atomic_fetch_add(&reads_ok, 1);
        } else if (r < 0 && errno == EBADF) {
            atomic_fetch_add(&reads_badf, 1);
        } else {
            atomic_fetch_add(&bad, 1);
        }
    }
    return 0;
}

static int make_file(const char *path, char c, size_t n) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) return -1;
    char *b = malloc(n);
    memset(b, c, n);
    if (write(fd, b, n) != (ssize_t)n) return -1;
    free(b);
    return fd;
}

static const char *files(void) {
    int fa = make_file("/tmp/cwr-a", 'A', 4096);
    int fb = make_file("/tmp/cwr-b", 'B', 4096);
    if (fa < 0 || fb < 0) return "setup failed";
    if (dup2(fa, TARGET) != TARGET) return "dup2 failed";
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, file_reader, 0);
    for (int i = 0; i < 3000; i++) {
        close(TARGET);
        if (dup2(i & 1 ? fa : fb, TARGET) != TARGET) atomic_fetch_add(&bad, 1);
        if (i % 3 == 0) {
            /* And an open that lands on the target: the lowest free
             * descriptor is not it, so make it so with a close first. */
            close(TARGET);
            int f = open(i & 2 ? "/tmp/cwr-a" : "/tmp/cwr-b", O_RDONLY);
            if (f < 0) atomic_fetch_add(&bad, 1);
            else if (f != TARGET) {
                dup2(f, TARGET);
                close(f);
            }
        }
    }
    atomic_store(&stop, 1);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    close(TARGET);
    close(fa);
    close(fb);
    unlink("/tmp/cwr-a");
    unlink("/tmp/cwr-b");
    if (atomic_load(&bad)) return "a read saw a mix, another errno or nothing";
    if (atomic_load(&reads_ok) == 0) return "no read succeeded";
    return 0;
}

/* -- offsets -------------------------------------------------------------- */

#define PIECES 4096
static int off_fd;
static atomic_uchar seen[PIECES];
static atomic_long off_bad;

static void *off_reader(void *arg) {
    (void)arg;
    unsigned char buf[16];
    for (;;) {
        ssize_t r = read(off_fd, buf, 16);
        if (r == 0) break;
        if (r != 16) {
            atomic_fetch_add(&off_bad, 1);
            break;
        }
        unsigned idx = buf[0] | buf[1] << 8;
        if (idx >= PIECES || buf[2] != (unsigned char)~buf[0] || atomic_fetch_add(&seen[idx], 1) != 0)
            atomic_fetch_add(&off_bad, 1);
    }
    return 0;
}

static const char *offsets(void) {
    int fd = open("/tmp/cwr-off", O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) return "setup failed";
    for (unsigned i = 0; i < PIECES; i++) {
        unsigned char p[16] = {i & 0xff, i >> 8, (unsigned char)~(i & 0xff)};
        if (write(fd, p, 16) != 16) return "setup write failed";
    }
    lseek(fd, 0, SEEK_SET);
    off_fd = fd;
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, off_reader, 0);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    close(fd);
    unlink("/tmp/cwr-off");
    for (int i = 0; i < PIECES; i++)
        if (atomic_load(&seen[i]) != 1) return "a piece was read twice or never";
    if (atomic_load(&off_bad)) return "a short or duplicated read";
    return 0;
}

/* -- eof ------------------------------------------------------------------ */

static int eof_rd;
static atomic_int eof_got = -2;

static void *eof_reader(void *arg) {
    (void)arg;
    char c;
    atomic_store(&eof_got, (int)read(eof_rd, &c, 1));
    return 0;
}

static int eof_w1, eof_w2;
static void *eof_closer(void *arg) {
    (void)arg;
    nap_ms(50);
    close(eof_w1);
    close(eof_w2);
    return 0;
}

static const char *eof(void) {
    int p[2];
    if (pipe(p)) return "pipe failed";
    eof_rd = p[0];
    eof_w1 = p[1];
    eof_w2 = dup(p[1]);
    pthread_t r, c;
    pthread_create(&r, 0, eof_reader, 0);
    nap_ms(20);
    pthread_create(&c, 0, eof_closer, 0);
    pthread_join(c, 0);
    for (int i = 0; i < 400 && atomic_load(&eof_got) == -2; i++) nap_ms(10);
    if (atomic_load(&eof_got) != 0) return "the reader did not see end of file";
    pthread_join(r, 0);
    close(p[0]);
    return 0;
}

/* -- pipes ---------------------------------------------------------------- */

/* Bytes of any length (a write past what the pipe holds is split, on Linux
 * too): each writer's bytes are its letter, and every one must arrive
 * exactly once. */
#define WRITERS 4
#define CHUNKS 2000
static int pp[2];
static atomic_long got[WRITERS], pipe_bad;

static long chunk_len(unsigned s) { return 1 + (s * 37) % 64; }

static void *pipe_writer(void *arg) {
    unsigned w = (unsigned)(long)arg;
    char buf[64];
    memset(buf, 'a' + w, sizeof buf);
    for (unsigned s = 0; s < CHUNKS; s++) {
        long len = chunk_len(s), done = 0;
        while (done < len) {
            ssize_t n;
            if (s & 1) {
                long h = (len - done) / 2;
                struct iovec v[2] = {{buf, h}, {buf, len - done - h}};
                n = writev(pp[1], v, 2);
            } else {
                n = write(pp[1], buf, len - done);
            }
            if (n <= 0) {
                atomic_fetch_add(&pipe_bad, 1);
                return 0;
            }
            done += n;
        }
    }
    return 0;
}

static void *pipe_reader(void *arg) {
    int vec = (int)(long)arg;
    char buf[100];
    for (;;) {
        ssize_t n;
        if (vec) {
            struct iovec v[2] = {{buf, 30}, {buf + 30, 70}};
            n = readv(pp[0], v, 2);
        } else {
            n = read(pp[0], buf, sizeof buf);
        }
        if (n == 0) break;
        if (n < 0) {
            atomic_fetch_add(&pipe_bad, 1);
            break;
        }
        for (ssize_t i = 0; i < n; i++) {
            unsigned w = (unsigned char)buf[i] - 'a';
            if (w < WRITERS) atomic_fetch_add(&got[w], 1);
            else atomic_fetch_add(&pipe_bad, 1);
        }
    }
    return 0;
}

static const char *pipes(void) {
    if (pipe(pp)) return "pipe failed";
    long want = 0;
    for (unsigned s = 0; s < CHUNKS; s++) want += chunk_len(s);
    pthread_t w[WRITERS], r[2];
    for (int i = 0; i < 2; i++) pthread_create(&r[i], 0, pipe_reader, (void *)(long)i);
    for (int i = 0; i < WRITERS; i++) pthread_create(&w[i], 0, pipe_writer, (void *)(long)i);
    for (int i = 0; i < WRITERS; i++) pthread_join(w[i], 0);
    close(pp[1]);
    for (int i = 0; i < 2; i++) pthread_join(r[i], 0);
    close(pp[0]);
    if (atomic_load(&pipe_bad)) return "a write or read failed, or a byte came from nowhere";
    for (int i = 0; i < WRITERS; i++)
        if (atomic_load(&got[i]) != want) return "a byte was lost or repeated";
    return 0;
}

/* -- records --------------------------------------------------------------- */

/* Writes of at most PIPE_BUF are whole: four writers send fixed-size
 * records with write and with a two-vector writev, and two readers taking
 * a record a read (read and readv) find every one whole and once. */
#define RECORDS 2000
struct rec {
    unsigned writer, seq, check;
    char pad[52];
};
static int rp[2];
static atomic_long rgot[WRITERS], rseqsum[WRITERS], rec_bad;

static void *rec_writer(void *arg) {
    unsigned w = (unsigned)(long)arg;
    for (unsigned s = 0; s < RECORDS; s++) {
        struct rec r = {w, s, w * 7919u ^ s};
        memset(r.pad, 'a' + w, sizeof r.pad);
        ssize_t n;
        if (s & 1) {
            struct iovec v[2] = {{&r, 12}, {(char *)&r + 12, sizeof r - 12}};
            n = writev(rp[1], v, 2);
        } else {
            n = write(rp[1], &r, sizeof r);
        }
        if (n != sizeof r) atomic_fetch_add(&rec_bad, 1);
    }
    return 0;
}

static void *rec_reader(void *arg) {
    int vec = (int)(long)arg;
    for (;;) {
        struct rec r;
        ssize_t n;
        if (vec) {
            struct iovec v[2] = {{&r, 8}, {(char *)&r + 8, sizeof r - 8}};
            n = readv(rp[0], v, 2);
        } else {
            n = read(rp[0], &r, sizeof r);
        }
        if (n == 0) break;
        if (n != sizeof r || r.writer >= WRITERS || r.check != (r.writer * 7919u ^ r.seq)) {
            atomic_fetch_add(&rec_bad, 1);
            continue;
        }
        atomic_fetch_add(&rgot[r.writer], 1);
        atomic_fetch_add(&rseqsum[r.writer], r.seq);
    }
    return 0;
}

static const char *records(void) {
    if (pipe(rp)) return "pipe failed";
    pthread_t w[WRITERS], r[2];
    for (int i = 0; i < 2; i++) pthread_create(&r[i], 0, rec_reader, (void *)(long)i);
    for (int i = 0; i < WRITERS; i++) pthread_create(&w[i], 0, rec_writer, (void *)(long)i);
    for (int i = 0; i < WRITERS; i++) pthread_join(w[i], 0);
    close(rp[1]);
    for (int i = 0; i < 2; i++) pthread_join(r[i], 0);
    close(rp[0]);
    if (atomic_load(&rec_bad)) return "a record arrived torn or a write was short";
    for (int i = 0; i < WRITERS; i++)
        if (atomic_load(&rgot[i]) != RECORDS || atomic_load(&rseqsum[i]) != (long)RECORDS * (RECORDS - 1) / 2)
            return "a record was lost or repeated";
    return 0;
}

/* -- sockets -------------------------------------------------------------- */

#define STREAM (256 * 1024)
static unsigned char byte_at(long i, int dir) { return (unsigned char)(i * 7 + (i >> 9) + dir * 101); }
struct end {
    int fd, dir;
    long bad;
};

static void *sock_writer(void *arg) {
    struct end *e = arg;
    unsigned char buf[700];
    long at = 0;
    int k = 0;
    while (at < STREAM) {
        int len = 1 + (k * 131) % 700;
        if (len > STREAM - at) len = STREAM - at;
        for (int i = 0; i < len; i++) buf[i] = byte_at(at + i, e->dir);
        ssize_t n;
        switch (k++ % 3) {
        case 0:
            n = write(e->fd, buf, len);
            break;
        case 1: {
            int h = len / 2;
            struct iovec v[2] = {{buf, h}, {buf + h, len - h}};
            n = writev(e->fd, v, 2);
            break;
        }
        default: {
            struct iovec v = {buf, len};
            struct msghdr m = {0};
            m.msg_iov = &v;
            m.msg_iovlen = 1;
            n = sendmsg(e->fd, &m, 0);
        }
        }
        if (n <= 0) {
            e->bad++;
            break;
        }
        at += n;
    }
    shutdown(e->fd, SHUT_WR);
    return 0;
}

static void *sock_reader(void *arg) {
    struct end *e = arg;
    unsigned char buf[900];
    long at = 0;
    int k = 0;
    for (;;) {
        int len = 1 + (k * 173) % 900;
        ssize_t n;
        switch (k++ % 3) {
        case 0:
            n = read(e->fd, buf, len);
            break;
        case 1: {
            int h = len / 3;
            struct iovec v[2] = {{buf, h}, {buf + h, len - h}};
            n = readv(e->fd, v, 2);
            break;
        }
        default: {
            struct iovec v = {buf, len};
            struct msghdr m = {0};
            m.msg_iov = &v;
            m.msg_iovlen = 1;
            n = recvmsg(e->fd, &m, 0);
        }
        }
        if (n == 0) break;
        if (n < 0) {
            e->bad++;
            break;
        }
        for (ssize_t i = 0; i < n; i++)
            if (buf[i] != byte_at(at + i, e->dir)) e->bad++;
        at += n;
    }
    if (at != STREAM) e->bad++;
    return 0;
}

static const char *sockets(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) return "socketpair failed";
    /* Each end writes its own direction from one thread and reads the
     * other's from another: four threads on two descriptors. */
    struct end wa = {sv[0], 0}, rb = {sv[1], 0}, wb = {sv[1], 1}, ra = {sv[0], 1};
    pthread_t t[4];
    pthread_create(&t[0], 0, sock_writer, &wa);
    pthread_create(&t[1], 0, sock_reader, &rb);
    pthread_create(&t[2], 0, sock_writer, &wb);
    pthread_create(&t[3], 0, sock_reader, &ra);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    close(sv[0]);
    close(sv[1]);
    if (wa.bad || wb.bad) return "a write failed";
    if (ra.bad || rb.bad) return "a byte arrived wrong, out of order or not at all";
    return 0;
}

int main(void) {
    struct {
        const char *name;
        const char *(*run)(void);
    } parts[] = {{"files", files}, {"offsets", offsets}, {"eof", eof}, {"pipes", pipes}, {"records", records}, {"sockets", sockets}};
    int fail = 0;
    for (unsigned i = 0; i < sizeof parts / sizeof parts[0]; i++) {
        const char *why = parts[i].run();
        printf("%s: %s\n", parts[i].name, why ? why : "ok");
        fflush(stdout);
        if (why) fail = 1;
    }
    return fail;
}
