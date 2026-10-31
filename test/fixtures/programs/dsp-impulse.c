/* The software round trip through /dev/dsp and the player's loopback
 * (the page's `?audioloop=1`, run-node's GEX_AUDIO_LOOP=1): a program that
 * keeps the device fed with silence, 128 frames a write, puts an impulse in
 * every half second and times its return on capture. S16 stereo at the
 * player's rate (SNDCTL_DSP_GEX_INFO says which), so nothing is resampled.
 *
 *   dsp-impulse [FRAGSHIFT [FRAGS]]   (default 9 2: 2 x 512 bytes, 256 frames)
 *
 * Prints "dsp-impulse info playing P rate R latency_us L quantum Q", then
 * "dsp-impulse rt ms MIN MED MAX (N)": from the write that held the impulse
 * returning to the read that brought it back, in ms of the monotonic clock
 * (to within one write's 2.7 ms). The browser's own latency (L) is not in
 * a digital loopback: add it for the total. */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define SNDCTL_DSP_SPEED 0xc0045002
#define SNDCTL_DSP_SETFMT 0xc0045005
#define SNDCTL_DSP_CHANNELS 0xc0045006
#define SNDCTL_DSP_SETFRAGMENT 0xc004500a
#define SNDCTL_DSP_GEX_INFO 0x801450e0
#define AFMT_S16_LE 0x10
#define CHUNK 128
#define IMPULSES 8

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static int setup(int fd, int rate, int frag) {
    int v = AFMT_S16_LE;
    if (ioctl(fd, SNDCTL_DSP_SETFMT, &v)) return -1;
    v = 2;
    if (ioctl(fd, SNDCTL_DSP_CHANNELS, &v)) return -1;
    v = rate;
    if (ioctl(fd, SNDCTL_DSP_SPEED, &v)) return -1;
    if (frag && ioctl(fd, SNDCTL_DSP_SETFRAGMENT, &frag)) return -1;
    return 0;
}

static int cmp(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

int main(int argc, char **argv) {
    int shift = argc > 1 ? atoi(argv[1]) : 9, frags = argc > 2 ? atoi(argv[2]) : 2;
    int w = open("/dev/dsp", O_WRONLY);
    int r = open("/dev/dsp", O_RDONLY | O_NONBLOCK);
    if (w < 0 || r < 0) { perror("open /dev/dsp"); return 1; }
    unsigned info[5] = {0};
    if (ioctl(w, SNDCTL_DSP_GEX_INFO, info)) { printf("dsp-impulse no player (%s)\n", strerror(errno)); return 2; }
    printf("dsp-impulse info playing %u rate %u latency_us %u quantum %u\n", info[0], info[1], info[2], info[3]);
    int rate = info[1] ? (int)info[1] : 48000;
    if (setup(w, rate, (frags << 16) | shift) || setup(r, rate, 0)) { perror("ioctl"); return 3; }
    static int16_t out[CHUNK * 2], in[4096 * 2];
    double sent[IMPULSES], rt[IMPULSES];
    int nsent = 0, nback = 0;
    long chunks = 0, every = rate / 2 / CHUNK;
    double t0 = now_ms();
    while (nback < IMPULSES && now_ms() - t0 < 20000) {
        memset(out, 0, sizeof out);
        int pulse = nsent == nback && nsent < IMPULSES && chunks > 0 && chunks % every == 0;
        if (pulse) out[0] = out[1] = 30000;
        const char *c = (const char *)out;
        size_t left = sizeof out;
        while (left) {
            ssize_t n = write(w, c, left);
            if (n < 0) { if (errno == EINTR) continue; perror("write"); return 4; }
            c += n; left -= n;
        }
        if (pulse) sent[nsent++] = now_ms();
        chunks++;
        for (;;) {
            ssize_t n = read(r, in, sizeof in);
            if (n <= 0) break;
            if (nback < nsent)
                for (ssize_t i = 0; i < n / 2; i++)
                    if (in[i] > 15000 || in[i] < -15000) { rt[nback] = now_ms() - sent[nback]; nback++; break; }
        }
    }
    if (nback == 0) { printf("dsp-impulse no impulse came back (%d sent)\n", nsent); return 5; }
    qsort(rt, nback, sizeof rt[0], cmp);
    printf("dsp-impulse rt ms %.1f %.1f %.1f (%d)\n", rt[0], rt[nback / 2], rt[nback - 1], nback);
    return 0;
}
