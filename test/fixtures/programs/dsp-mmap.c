/* OSS output through a mapped DMA buffer, in the order Quake-engine games
 * set it up: open /dev/dsp read-write, reset, the capabilities (trigger and
 * mmap both needed), the output space (the buffer's size), a shared
 * PROT_WRITE mapping of the whole buffer, the format, the trigger off and
 * on, then the output pointer (SNDCTL_DSP_GETOPTR) as the place the device
 * is playing. It mixes a 441 Hz tone at half amplitude a little ahead of
 * the pointer for 1.5 s; the pointer's byte count must advance at the
 * stream's rate. Prints "dsp-mmap ok" and exits 0, or names the step that
 * failed. */
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/soundcard.h>
#include <time.h>
#include <unistd.h>

#define RATE 22050

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static int fail(const char *step) {
    printf("dsp-mmap FAIL %s: %s\n", step, strerror(errno));
    return 1;
}

int main(void) {
    int fd = open("/dev/dsp", O_RDWR);
    if (fd < 0) return fail("open");
    if (ioctl(fd, SNDCTL_DSP_RESET, 0) < 0) return fail("SNDCTL_DSP_RESET");
    int caps = 0;
    if (ioctl(fd, SNDCTL_DSP_GETCAPS, &caps) < 0) return fail("SNDCTL_DSP_GETCAPS");
    if (!(caps & DSP_CAP_TRIGGER) || !(caps & DSP_CAP_MMAP)) {
        printf("dsp-mmap FAIL caps 0x%x: no trigger or no mmap\n", caps);
        return 1;
    }
    audio_buf_info info;
    if (ioctl(fd, SNDCTL_DSP_GETOSPACE, &info) < 0) return fail("SNDCTL_DSP_GETOSPACE");
    int size = info.fragstotal * info.fragsize;
    if (size <= 0) {
        printf("dsp-mmap FAIL buffer %d fragments of %d bytes\n", info.fragstotal, info.fragsize);
        return 1;
    }
    short *buf = mmap(NULL, size, PROT_WRITE, MAP_FILE | MAP_SHARED, fd, 0);
    if (buf == MAP_FAILED) return fail("mmap");
    int fmt = AFMT_S16_LE, stereo = 1, rate = RATE;
    if (ioctl(fd, SNDCTL_DSP_SETFMT, &fmt) < 0 || fmt != AFMT_S16_LE) return fail("SNDCTL_DSP_SETFMT");
    if (ioctl(fd, SNDCTL_DSP_STEREO, &stereo) < 0 || stereo != 1) return fail("SNDCTL_DSP_STEREO");
    if (ioctl(fd, SNDCTL_DSP_SPEED, &rate) < 0) return fail("SNDCTL_DSP_SPEED");
    int frames = size / 4; /* the buffer in stereo 16-bit frames */
    memset(buf, 0, size);
    int trig = 0;
    if (ioctl(fd, SNDCTL_DSP_SETTRIGGER, &trig) < 0) return fail("SNDCTL_DSP_SETTRIGGER off");
    trig = PCM_ENABLE_OUTPUT;
    if (ioctl(fd, SNDCTL_DSP_SETTRIGGER, &trig) < 0) return fail("SNDCTL_DSP_SETTRIGGER on");
    count_info c;
    if (ioctl(fd, SNDCTL_DSP_GETOPTR, &c) < 0) return fail("SNDCTL_DSP_GETOPTR");
    int first = c.bytes;
    double t0 = now();
    long painted = c.ptr / 4; /* frames mixed, counted from the buffer's start */
    long base = 0;           /* frames before the buffer's current lap */
    int last = c.ptr / 4;
    while (now() - t0 < 1.5) {
        if (ioctl(fd, SNDCTL_DSP_GETOPTR, &c) < 0) return fail("SNDCTL_DSP_GETOPTR");
        int at = c.ptr / 4;
        if (at < last) base += frames;
        last = at;
        long played = base + at;
        /* about 50 ms ahead of the pointer, never a whole lap */
        long ahead = played + RATE / 20;
        if (ahead - played >= frames) ahead = played + frames - 1;
        for (; painted < ahead; painted++) {
            short v = (short)(16384 * sin(2 * M_PI * 441.0 * painted / RATE));
            buf[(painted % frames) * 2] = v;
            buf[(painted % frames) * 2 + 1] = v;
        }
        struct timespec d = {0, 5 * 1000 * 1000};
        nanosleep(&d, NULL);
    }
    double secs = now() - t0;
    if (ioctl(fd, SNDCTL_DSP_GETOPTR, &c) < 0) return fail("SNDCTL_DSP_GETOPTR");
    double moved = (double)(c.bytes - first) / 4 / RATE; /* seconds of audio */
    if (moved < secs * 0.7 || moved > secs * 1.3) {
        printf("dsp-mmap FAIL pointer: %.2f s of audio in %.2f s\n", moved, secs);
        return 1;
    }
    munmap(buf, size);
    close(fd);
    printf("dsp-mmap ok\n");
    return 0;
}
