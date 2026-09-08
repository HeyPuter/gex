/* glBlitFramebuffer under the scissor test.
 *
 * GL says the blit is affected by the scissor: texels outside the box keep
 * whatever the destination held. That is easy to lose in a driver, because a
 * same-format same-size blit is otherwise a plain texture copy and a copy has
 * no scissor -- so the scissored case has to be routed to a *draw* instead,
 * and then the draw has to actually pick the box up.
 *
 * Both halves are checked, because either alone passes for the wrong reason:
 * a driver that ignores the scissor passes `inside` and fails `outside`, and
 * one that drops the blit entirely passes `outside` and fails `inside`.
 *
 *   inside_scissor    a texel inside the box takes the source's colour
 *   outside_scissor   a texel outside it keeps the destination's
 *   scissor_off       and with the test disabled the whole blit lands
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 32
/* The box covers the lower-left quadrant, so the centre of each quadrant is
 * unambiguously in or out. */
#define BOX 16

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint make_fbo(GLuint *tex, float r, float g, float b)
{
    glGenTextures(1, tex);
    glBindTexture(GL_TEXTURE_2D, *tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint fbo = 0;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, *tex, 0);
    glDisable(GL_SCISSOR_TEST);
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(r, g, b, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    return fbo;
}

static unsigned read_at(GLuint fbo, int x, int y, unsigned char px[4])
{
    glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
    glDisable(GL_SCISSOR_TEST);
    glReadPixels(x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    return glGetError() == GL_NO_ERROR;
}

int main(void)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 1; }
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                                       EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, NULL);
    if (ctx == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 1; }
    static const EGLint surf_attrs[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);

    /* Source is red, destination green, so a texel names where it came from. */
    GLuint src_tex = 0, dst_tex = 0;
    const GLuint src = make_fbo(&src_tex, 1.0f, 0.0f, 0.0f);
    const GLuint dst = make_fbo(&dst_tex, 0.0f, 1.0f, 0.0f);

    glEnable(GL_SCISSOR_TEST);
    glScissor(0, 0, BOX, BOX);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, src);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dst);
    glBlitFramebuffer(0, 0, SIDE, SIDE, 0, 0, SIDE, SIDE,
                      GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glDisable(GL_SCISSOR_TEST);

    unsigned char in_px[4] = {0}, out_px[4] = {0};
    const int got_in = read_at(dst, BOX / 2, BOX / 2, in_px);
    const int got_out = read_at(dst, SIDE - BOX / 2, SIDE - BOX / 2, out_px);
    fprintf(stderr, "INFO inside=%u,%u,%u outside=%u,%u,%u\n",
            in_px[0], in_px[1], in_px[2], out_px[0], out_px[1], out_px[2]);
    /* Inside: red, from the source. Outside: green, the destination's clear. */
    result("inside_scissor", got_in && in_px[0] > 200 && in_px[1] < 60);
    result("outside_scissor", got_out && out_px[1] > 200 && out_px[0] < 60);

    /* And with the test off the whole blit lands, which separates "the
     * scissor was applied" from "the blit did nothing". */
    glBindFramebuffer(GL_FRAMEBUFFER, dst);
    glDisable(GL_SCISSOR_TEST);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, src);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dst);
    glBlitFramebuffer(0, 0, SIDE, SIDE, 0, 0, SIDE, SIDE,
                      GL_COLOR_BUFFER_BIT, GL_NEAREST);
    unsigned char off_px[4] = {0};
    const int got_off = read_at(dst, SIDE - BOX / 2, SIDE - BOX / 2, off_px);
    fprintf(stderr, "INFO scissor_off=%u,%u,%u\n", off_px[0], off_px[1],
            off_px[2]);
    result("scissor_off", got_off && off_px[0] > 200 && off_px[1] < 60);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
