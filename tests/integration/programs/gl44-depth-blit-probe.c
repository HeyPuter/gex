/* glBlitFramebuffer of GL_DEPTH_BUFFER_BIT through the raster path.
 *
 * A same-size unscissored depth blit is a plain whole-subresource copy, so it
 * cannot see this: the interesting cases are the ones that have to be *drawn*,
 * because a drawn depth blit samples the source depth and writes it out of
 * gl_FragDepth. If that store is lost the quad's own interpolated z survives
 * instead -- and u_blitter draws its quad at NDC z=0, which is window depth
 * 0.5, a value that looks like a plausible depth rather than like a bug.
 *
 *   scissored   full size, scissor test on  -- forced to a draw by the scissor
 *   scaled      half size into full         -- forced to a draw by the scale
 *   unscissored the copy path, as a control
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>

#define SIDE 64
#define HALF 32

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint make_fbo(GLuint *tex, int side, float depth)
{
    glGenTextures(1, tex);
    glBindTexture(GL_TEXTURE_2D, *tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT24, side, side, 0,
                 GL_DEPTH_COMPONENT, GL_FLOAT, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    GLuint fbo = 0;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                           *tex, 0);
    glDisable(GL_SCISSOR_TEST);
    glViewport(0, 0, side, side);
    glClearDepth(depth);
    glClear(GL_DEPTH_BUFFER_BIT);
    return fbo;
}

static float read_depth(GLuint fbo, int x, int y)
{
    float d = -1.0f;
    glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
    glDisable(GL_SCISSOR_TEST);
    glReadPixels(x, y, 1, 1, GL_DEPTH_COMPONENT, GL_FLOAT, &d);
    return d;
}

/* The source is 0.25 and every destination 0.75, so a lost store shows up as
 * 0.5 -- distinct from both, and from a blit that did nothing. */
#define SRC_DEPTH 0.25f
#define DST_DEPTH 0.75f

static int near(float got, float want) { return fabsf(got - want) < 0.01f; }

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

    GLuint src_tex = 0, dst_tex = 0, small_tex = 0;
    const GLuint src = make_fbo(&src_tex, SIDE, SRC_DEPTH);
    const GLuint dst = make_fbo(&dst_tex, SIDE, DST_DEPTH);
    const GLuint small = make_fbo(&small_tex, HALF, SRC_DEPTH);

    /* 1. scissored: only the lower-left quadrant may change. */
    glEnable(GL_SCISSOR_TEST);
    glScissor(0, 0, HALF, HALF);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, src);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dst);
    glBlitFramebuffer(0, 0, SIDE, SIDE, 0, 0, SIDE, SIDE,
                      GL_DEPTH_BUFFER_BIT, GL_NEAREST);
    glDisable(GL_SCISSOR_TEST);
    const float in_d = read_depth(dst, HALF / 2, HALF / 2);
    const float out_d = read_depth(dst, SIDE - HALF / 2, SIDE - HALF / 2);
    fprintf(stderr, "INFO scissored inside=%.4f outside=%.4f\n", in_d, out_d);
    result("scissored_inside", near(in_d, SRC_DEPTH));
    result("scissored_outside", near(out_d, DST_DEPTH));

    /* 2. scaled: a half-size source stretched over the whole destination. */
    glBindFramebuffer(GL_FRAMEBUFFER, dst);
    glClearDepth(DST_DEPTH);
    glClear(GL_DEPTH_BUFFER_BIT);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, small);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dst);
    glBlitFramebuffer(0, 0, HALF, HALF, 0, 0, SIDE, SIDE,
                      GL_DEPTH_BUFFER_BIT, GL_NEAREST);
    const float scaled_d = read_depth(dst, SIDE / 2, SIDE / 2);
    fprintf(stderr, "INFO scaled=%.4f\n", scaled_d);
    result("scaled", near(scaled_d, SRC_DEPTH));

    /* 3. the plain copy, as a control that the setup itself is sound. */
    glBindFramebuffer(GL_FRAMEBUFFER, dst);
    glClearDepth(DST_DEPTH);
    glClear(GL_DEPTH_BUFFER_BIT);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, src);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, dst);
    glBlitFramebuffer(0, 0, SIDE, SIDE, 0, 0, SIDE, SIDE,
                      GL_DEPTH_BUFFER_BIT, GL_NEAREST);
    const float copy_d = read_depth(dst, SIDE / 2, SIDE / 2);
    fprintf(stderr, "INFO unscissored=%.4f\n", copy_d);
    result("unscissored", near(copy_d, SRC_DEPTH));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
