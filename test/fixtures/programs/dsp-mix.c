/* Two programs' worth of /dev/dsp at once: two opens play sines (441 Hz and
 * 1000 Hz, amplitude 0.25, S16 stereo at the player's rate) side by side,
 * and the player's loopback (GEX_AUDIO_LOOP=1) is read back. Mixed, the
 * capture's RMS is 0.25; one stream alone (the old "last writer takes the
 * device") would be 0.177. Prints "dsp-mix rms R". */
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
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

static int setup(int fd, int rate) {
    int v = AFMT_S16_LE;
    if (ioctl(fd, SNDCTL_DSP_SETFMT, &v)) return -1;
    v = 2;
    if (ioctl(fd, SNDCTL_DSP_CHANNELS, &v)) return -1;
    v = rate;
    if (ioctl(fd, SNDCTL_DSP_SPEED, &v)) return -1;
    v = (4 << 16) | 9;
    return ioctl(fd, SNDCTL_DSP_SETFRAGMENT, &v);
}

static void put(int fd, const int16_t *b) {
    const char *c = (const char *)b;
    size_t left = CHUNK * 4;
    while (left) {
        ssize_t n = write(fd, c, left);
        if (n < 0) { if (errno == EINTR) continue; perror("write"); _exit(4); }
        c += n; left -= n;
    }
}

int main(void) {
    int a = open("/dev/dsp", O_WRONLY), b = open("/dev/dsp", O_WRONLY), r = open("/dev/dsp", O_RDONLY | O_NONBLOCK);
    if (a < 0 || b < 0 || r < 0) { perror("open"); return 1; }
    unsigned info[5] = {0};
    if (ioctl(a, SNDCTL_DSP_GEX_INFO, info)) { printf("dsp-mix no player\n"); return 2; }
    int rate = info[1] ? (int)info[1] : 48000;
    if (setup(a, rate) || setup(b, rate) || setup(r, rate)) { perror("ioctl"); return 3; }
    static int16_t x[CHUNK * 2], y[CHUNK * 2], in[4096 * 2];
    double sq = 0;
    long n = 0, t = 0;
    for (int k = 0; k < rate / CHUNK; k++) {
        for (int i = 0; i < CHUNK; i++, t++) {
            x[2 * i] = x[2 * i + 1] = (int16_t)(8192 * sin(2 * M_PI * 441 * t / rate));
            y[2 * i] = y[2 * i + 1] = (int16_t)(8192 * sin(2 * M_PI * 1000 * t / rate));
        }
        put(a, x);
        put(b, y);
        for (;;) {
            ssize_t got = read(r, in, sizeof in);
            if (got <= 0) break;
            // The second half of the second only: both streams going.
            if (k > rate / CHUNK / 2)
                for (ssize_t i = 0; i < got / 2; i += 2) { double v = in[i] / 32768.0; sq += v * v; n++; }
        }
    }
    printf("dsp-mix rms %.3f (%ld frames)\n", n ? sqrt(sq / n) : 0, n);
    return 0;
}
