/* glPolygonMode with the two faces set differently.
 *
 * GL lets the front and back faces be filled, outlined or stippled
 * independently; WebGPU has no polygon mode at all, so this driver rewrites
 * an unfilled polygon into the lines or points it would be drawn as, and used
 * to *refuse* the state outright when the two faces disagreed. A refused
 * rasterizer state binds nothing, so those programs lost culling and the
 * scissor as well and drew filled and wrong. Eight programs in the app sweep
 * ask for it and it was the only rasterizer state left refused:
 * dlist-edgeflag, dlist-edgeflag-dangling, tri-edgeflag, tri-edgeflag-pv,
 * tri-edgeflag-array, tri-unfilled-smooth, pgon-mode and vp-unfilled.
 *
 * It is drawn as one pass per facing now, each with a single fill mode and
 * the opposite face culled, so what has to be checked is that *both* halves
 * land in one draw under one state -- and that the application's own cull
 * state composes with the split rather than being replaced by it.
 *
 * Two triangles of opposite winding are drawn together, left and right, so
 * each facing can be measured on its own. Area is what separates "outlined"
 * from "filled"; no single pixel says it.
 *
 * A **compatibility** context, deliberately: GL 3.2 core removed per-face
 * polygon mode, and glPolygonMode(GL_FRONT, ...) there is a GL_INVALID_ENUM.
 * The first version of this lived in the core-profile polygon-mode probe and
 * both drivers "agreed" at 3/6 because neither had accepted the state at all.
 *
 * KNOWN GAP -- `cull_back_with_split` is expected to fail on gex_wgpu, 5/6
 * against llvmpipe's 6/6, and the failure is real rather than a probe
 * artefact. Each pass of the split culls the other face, but WebGPU applies
 * cullMode only to triangle topology and an unfilled pass has been rewritten
 * into lines or points by then, so the cull is inert and the pass draws both
 * facings' edges. gex_wgpu_draw_unfilled documents the same gap for the
 * uniform-fill-mode case ("Lines carry no winding [...] the alternative is
 * culling on the CPU"), and one fix repairs both: capture the post-VS
 * positions with the compute path the wide-line rewrite already uses, compute
 * each source primitive's window-space signed area, and hand the unfilled
 * translator only the primitives of the wanted facing.
 *
 * The gap is small in practice and worth measuring before spending that: the
 * eight affected programs went from 0.46 to 0.02-0.07 against llvmpipe when
 * the state stopped being refused, and three of them are now at or below the
 * llvmpipe-vs-softpipe floor. What is left is the wrong facing's vertex
 * points and edges, which mostly land on top of geometry that is already
 * there.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 64

/* Counter-clockwise on the left, clockwise on the right: under GL_CCW the
 * left triangle is front-facing and the right one is back-facing. Each stays
 * inside its own half so the two can be counted separately. */
static void draw_pair(void) {
    glBegin(GL_TRIANGLES);
    glVertex2f(-0.95f, -0.95f); glVertex2f(-0.10f, -0.95f);
    glVertex2f(-0.525f, 0.95f);

    glVertex2f( 0.10f, -0.95f); glVertex2f( 0.525f, 0.95f);
    glVertex2f( 0.95f, -0.95f);
    glEnd();
}

static void halves(unsigned char *image, unsigned *left, unsigned *right) {
    *left = *right = 0;
    for (int y = 0; y < SIDE; ++y)
        for (int x = 0; x < SIDE; ++x)
            if (image[((size_t)y * SIDE + x) * 4] > 128) {
                if (x < SIDE / 2) ++*left; else ++*right;
            }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint surface_attributes[] = {
        EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE,
    };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    glViewport(0, 0, SIDE, SIDE);
    glColor3f(1.0f, 1.0f, 1.0f);
    glFrontFace(GL_CCW);
    glDisable(GL_CULL_FACE);

    unsigned char *image = malloc((size_t)SIDE * SIDE * 4);
    if (!image) return 2;
    const unsigned half = (unsigned)SIDE * SIDE / 2;
    unsigned left = 0, right = 0;

    /* A filled triangle covers a large part of its half; an outlined one is a
     * perimeter, so a small part. The two bounds are far apart, and a state
     * that was refused draws both filled and fails whichever half was asked
     * to be an outline. */
    struct { const char *name; GLenum front, back; int front_filled; }
    tests[] = {
        {"front_line_back_fill",  GL_LINE,  GL_FILL,  0},
        {"front_fill_back_line",  GL_FILL,  GL_LINE,  1},
        {"front_point_back_fill", GL_POINT, GL_FILL,  0},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        glPolygonMode(GL_FRONT, tests[i].front);
        glPolygonMode(GL_BACK, tests[i].back);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        draw_pair();
        glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);
        halves(image, &left, &right);
        const GLenum error = glGetError();
        const unsigned filled = tests[i].front_filled ? left : right;
        const unsigned sparse = tests[i].front_filled ? right : left;
        const int ok = error == GL_NO_ERROR && filled > half / 4 &&
                       sparse > 0 && sparse < half / 5;
        fprintf(stderr, "INFO %s left=%u right=%u filled=%u sparse=%u "
                "half=%u err=0x%04x\n", tests[i].name, left, right, filled,
                sparse, half, (unsigned)error);
        result(tests[i].name, ok);
    }

    /* The application's own cull state composes with the split rather than
     * being replaced by it. Culling the back face has to leave the
     * front-facing triangle, still outlined, and nothing on the right. A
     * driver that overwrote the cull state with the split's own would draw
     * the back-facing triangle too. */
    glPolygonMode(GL_FRONT, GL_LINE);
    glPolygonMode(GL_BACK, GL_FILL);
    glEnable(GL_CULL_FACE);
    glCullFace(GL_BACK);
    glClear(GL_COLOR_BUFFER_BIT);
    draw_pair();
    glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);
    halves(image, &left, &right);
    {
        const GLenum error = glGetError();
        const int ok = error == GL_NO_ERROR && left > 0 && left < half / 5 &&
                       right == 0;
        fprintf(stderr, "INFO cull_back_with_split left=%u right=%u "
                "err=0x%04x\n", left, right, (unsigned)error);
        result("cull_back_with_split", ok);
    }

    /* And the other way: culling the front leaves only the filled half. */
    glCullFace(GL_FRONT);
    glClear(GL_COLOR_BUFFER_BIT);
    draw_pair();
    glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);
    halves(image, &left, &right);
    {
        const GLenum error = glGetError();
        const int ok = error == GL_NO_ERROR && left == 0 && right > half / 4;
        fprintf(stderr, "INFO cull_front_with_split left=%u right=%u "
                "err=0x%04x\n", left, right, (unsigned)error);
        result("cull_front_with_split", ok);
    }
    glDisable(GL_CULL_FACE);

    /* Both faces agreeing still has to work, since the split must not fire
     * for it -- the control on the whole mechanism. */
    glPolygonMode(GL_FRONT_AND_BACK, GL_LINE);
    glClear(GL_COLOR_BUFFER_BIT);
    draw_pair();
    glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, image);
    halves(image, &left, &right);
    {
        const GLenum error = glGetError();
        const int ok = error == GL_NO_ERROR && left > 0 && left < half / 5 &&
                       right > 0 && right < half / 5;
        fprintf(stderr, "INFO both_line left=%u right=%u err=0x%04x\n", left,
                right, (unsigned)error);
        result("both_faces_line", ok);
    }
    glPolygonMode(GL_FRONT_AND_BACK, GL_FILL);
    free(image);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
