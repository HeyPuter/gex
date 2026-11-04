/* Known tones through /dev/dsp, for the page's audio ring
 * (hosts/kernel/src/audio.rs; test/page/audio.test.js reads it back);
 * a host without /dev/dsp cannot be its reference:
 * 441 Hz, S16 stereo at 44100 Hz for 0.5 s, then, after the device has been
 * quiet long enough for another stream to take it, a second open playing
 * 882 Hz as F32 mono at 48000 Hz for 0.5 s. Both at half amplitude (RMS
 * 0.354). The second stream's format and rate are set after a first write
 * in the old format, as a program changing its mind mid-stream would. */
#include <fcntl.h>
#include <errno.h>
#include <math.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define SNDCTL_DSP_SPEED 0xc0045002
#define SNDCTL_DSP_SETFMT 0xc0045005
#define SNDCTL_DSP_CHANNELS 0xc0045006
#define AFMT_S16_LE 0x10
#define AFMT_F32_LE 0x10000000

static int set(int fd, unsigned long req, int v) {
    int x = v;
    return ioctl(fd, req, &x) == 0 && x == v ? 0 : -1;
}

/* All of it: the device takes what fits and says EAGAIN when full. */
static int put(int fd, const void *p, size_t n) {
    const char *c = p;
    while (n) {
        ssize_t w = write(fd, c, n);
        if (w < 0 && errno == EAGAIN) {
            struct pollfd q = {fd, POLLOUT, 0};
            poll(&q, 1, 100);
            continue;
        }
        if (w <= 0) return -1;
        c += w;
        n -= (size_t)w;
    }
    return 0;
}

int main(void) {
    int fd = open("/dev/dsp", O_WRONLY);
    if (fd < 0) { perror("open /dev/dsp"); return 1; }
    if (set(fd, SNDCTL_DSP_SETFMT, AFMT_S16_LE) || set(fd, SNDCTL_DSP_CHANNELS, 2) || set(fd, SNDCTL_DSP_SPEED, 44100)) return 2;
    static int16_t s16[2 * 441];
    for (int i = 0; i < 441; i++) s16[2 * i] = s16[2 * i + 1] = (int16_t)lrint(16384 * sin(2 * M_PI * i / 100.0));
    for (int k = 0; k < 50; k++)
        if (put(fd, s16, sizeof s16)) return 3;
    close(fd);
    struct timespec quiet = {0, 400000000};
    nanosleep(&quiet, 0);
    fd = open("/dev/dsp", O_WRONLY);
    if (fd < 0) return 4;
    if (set(fd, SNDCTL_DSP_SETFMT, AFMT_S16_LE) || set(fd, SNDCTL_DSP_CHANNELS, 2) || set(fd, SNDCTL_DSP_SPEED, 44100)) return 5;
    if (put(fd, s16, sizeof s16)) return 6;
    if (set(fd, SNDCTL_DSP_SETFMT, AFMT_F32_LE) || set(fd, SNDCTL_DSP_CHANNELS, 1) || set(fd, SNDCTL_DSP_SPEED, 48000)) return 7;
    static float f32[480];
    for (int i = 0; i < 480; i++) f32[i] = 0.5f * (float)sin(2 * M_PI * i * 882.0 / 48000.0);
    for (int k = 0; k < 50; k++)
        if (put(fd, f32, sizeof f32)) return 8;
    close(fd);
    puts("dsp-tone ok");
    return 0;
}
