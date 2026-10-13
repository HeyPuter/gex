/* Two GL clients, one of which swaps and then goes idle.
 *
 * A swap at a swap interval above 0 hands its frame to the host without
 * waiting for it (gexgl's eglSwapBuffers, GEX_GL_PRESENT_ASYNC). The host's
 * ring is one buffer that every GL process claims in turn, and an unwaited
 * batch used to keep its claim until its client's next submit -- for a
 * program that swaps and then sleeps, until it wakes, while every other GL
 * client waited (thirty seconds, then stole the ring). The page now releases
 * the claim itself once the batch has run.
 *
 * The child (A) draws and swaps once, then sleeps; the parent (B) draws and
 * swaps 30 frames meanwhile, and must finish them before A wakes. Then A
 * swaps again. Under a claim kept by A's unwaited batch, B's first swap
 * waits until A wakes -- and fails the second case.
 *
 * The same source runs natively against Mesa for the expected SUMMARY. */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static GLuint framebuffer;

static int setup(void) {
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 0;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &count) || count < 1) return 0;
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE) return 0;
    if (!eglMakeCurrent(display, surface, surface, context)) return 0;
    /* An FBO, as the other probes here: one unambiguous attachment. */
    GLuint color = 0;
    glGenRenderbuffers(1, &color);
    glBindRenderbuffer(GL_RENDERBUFFER, color);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 8, 8);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, color);
    return glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
}

/* Clear to `v` and read it back. Before any swap: after one (any client's,
   of a pbuffer), this host's FBO reads back zero, with the waited swap as
   well (2026-10-13) -- not what this probe is about. */
static int reads(int v) {
    unsigned char p[4] = {0};
    glClearColor(v / 255.0f, v / 255.0f, v / 255.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, p);
    if (abs(p[0] - v) > 2 || abs(p[1] - v) > 2 || abs(p[2] - v) > 2)
        fprintf(stderr, "pid %d read %u,%u,%u, want %d\n", (int)getpid(), p[0], p[1], p[2], v);
    return abs(p[0] - v) <= 2 && abs(p[1] - v) <= 2 && abs(p[2] - v) <= 2;
}

/* Clear to `v`/255 grey and swap; whether the swap said yes. */
static int frame(int v) {
    glClearColor(v / 255.0f, v / 255.0f, v / 255.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    return eglSwapBuffers(eglGetCurrentDisplay(), eglGetCurrentSurface(EGL_DRAW)) == EGL_TRUE;
}

static int failed;
static void report(const char *what, int ok) {
    printf("%s: %s\n", what, ok ? "ok" : "FAIL");
    failed += !ok;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    int awake[2], ready[2];
    if (pipe(awake) || pipe(ready)) return 1;
    pid_t a = fork();
    if (a == 0) {
        /* A: one frame, then idle with its last batch unwaited. */
        int ok = setup() && reads(40) && frame(40);
        if (write(ready[1], ok ? "y" : "n", 1) != 1) _exit(1);
        sleep(6);
        if (write(awake[1], "w", 1) != 1) _exit(1);
        if (!ok) _exit(3);
        _exit(frame(210) ? 0 : 2);
    }
    close(awake[1]);
    close(ready[1]);
    char c = 0;
    if (read(ready[0], &c, 1) != 1) c = 'n';
    report("client A swaps", c == 'y');

    /* No readback here: A has swapped by now, and after another client's
       swap of a pbuffer this host reads this one's FBO back as zero too. */
    report("client B sets up", setup());
    int ok = 1;
    for (int i = 0; i < 30; i++) ok &= frame(i * 8);
    /* Not timed against a number: what matters is that A, asleep holding
       nothing, did not hold B up -- so B is done before A wakes. */
    fcntl(awake[0], F_SETFL, O_NONBLOCK);
    const int a_woke = read(awake[0], &c, 1) == 1;
    if (a_woke) fprintf(stderr, "A woke before B was done\n");
    report("client B draws 30 frames while A is idle", ok && !a_woke);

    int status = 0;
    waitpid(a, &status, 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status)) fprintf(stderr, "A ended with status %#x\n", status);
    report("client A swaps again after idling", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    printf("SUMMARY cases=4 failed=%d\n", failed);
    return failed != 0;
}
