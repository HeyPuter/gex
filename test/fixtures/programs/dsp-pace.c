/* How fast /dev/dsp takes a second of audio (S16 stereo, 44.1 kHz, 441
 * frames a write): the device paces its writer to whoever plays it, the
 * wall clock or the page's player (test/page/audio.test.js runs it with
 * run-node's stand-in player at four times and half the real rate). Prints
 * "dsp-pace ms N": from the first write to the last one taken. */
#include <errno.h>
#include <fcntl.h>
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

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

int main(void) {
    int fd = open("/dev/dsp", O_WRONLY);
    if (fd < 0) { perror("open /dev/dsp"); return 1; }
    int v = AFMT_S16_LE;
    if (ioctl(fd, SNDCTL_DSP_SETFMT, &v)) return 2;
    v = 2;
    if (ioctl(fd, SNDCTL_DSP_CHANNELS, &v)) return 2;
    v = 44100;
    if (ioctl(fd, SNDCTL_DSP_SPEED, &v)) return 2;
    static int16_t buf[2 * 441];
    for (int i = 0; i < 441; i++) buf[2 * i] = buf[2 * i + 1] = (int16_t)((i % 100) * 300 - 15000);
    double t0 = now_ms();
    for (int k = 0; k < 100; k++) {
        const char *c = (const char *)buf;
        size_t n = sizeof buf;
        while (n) {
            ssize_t w = write(fd, c, n);
            if (w < 0 && errno == EAGAIN) {
                struct pollfd q = {fd, POLLOUT, 0};
                poll(&q, 1, 100);
                continue;
            }
            if (w <= 0) return 3;
            c += w;
            n -= (size_t)w;
        }
    }
    printf("dsp-pace ms %.0f\n", now_ms() - t0);
    close(fd);
    return 0;
}
