/* ARB_viewport_array -- GL 4.1's only real remaining item, and the source of
 * ARB_fragment_layer_viewport, which is one of GL 4.3's.
 *
 * The extension turns the viewport, the scissor box and the depth range from
 * single pieces of state into arrays of at least sixteen, and lets a geometry
 * shader pick which one a primitive is rasterized through by writing
 * `gl_ViewportIndex`. Shadow-map atlases, cube-map faces rendered in one pass
 * and stereo rendering all use that; everything else uses index zero and never
 * notices the extension exists.
 *
 * That split is the whole shape of the problem on WebGPU, which has exactly
 * one viewport per render pass. **Index zero is not an approximation** -- GL
 * says a primitive with no shader-written index is rasterized through viewport
 * zero, so a driver with one viewport is *correct* for every program that does
 * not write `gl_ViewportIndex`, which is nearly all of them. What it cannot do
 * is route.
 *
 * So the cases below separate the two claims deliberately:
 *
 *   `count`             GL_MAX_VIEWPORTS is at least sixteen
 *   `roundtrip`         viewport, scissor and depth range at index 3 read
 *                       back as they were set -- state, with no rasterization
 *                       involved
 *   `indexed-zero`      glViewportIndexedf(0, ...) covers exactly its
 *                       rectangle, which is glViewport reached the other way
 *   `scissor-indexed`   glScissorIndexed(0, ...) clips to its box
 *   `depth-indexed`     glDepthRangeIndexed(0, ...) is honoured, checked
 *                       through a depth test rather than by reading depth
 *   `gs-route`          a geometry shader writing gl_ViewportIndex = 1 is
 *                       rasterized through viewport *one*, whose rectangle is
 *                       disjoint from viewport zero's
 *
 * The first five are what a one-viewport driver can serve correctly. The last
 * is what it cannot, and it is here so that a partial implementation reports
 * 5/6 rather than passing while silently drawing through the wrong viewport --
 * which is a whole-screen error, not a subtle one.
 *
 * Coverage is measured as a bounding box over the colour buffer, not as a
 * single pixel: a viewport that is the right size in the wrong place, or the
 * right place at the wrong size, both read as the correct colour somewhere.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
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

static int have_extension(const char *want)
{
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 32

static GLuint build(const char *gs_src)
{
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 330 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(1.0, 1.0, 1.0, 1.0); }\n";
    GLuint prog = glCreateProgram();
    struct { GLenum stage; const char *src; const char *what; } parts[] = {
        {GL_VERTEX_SHADER, vs_src, "vs"},
        {GL_GEOMETRY_SHADER, gs_src, "gs"},
        {GL_FRAGMENT_SHADER, fs_src, "fs"},
    };
    for (unsigned i = 0; i < 3; ++i) {
        if (!parts[i].src)
            continue;
        GLuint sh = glCreateShader(parts[i].stage);
        if (!sh) {
            glDeleteProgram(prog);
            return 0;
        }
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s compile: %s\n", parts[i].what, log);
            glDeleteShader(sh);
            glDeleteProgram(prog);
            return 0;
        }
        glAttachShader(prog, sh);
        glDeleteShader(sh);
    }
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

/* The bounding box of the covered pixels, and how many there were. A viewport
 * that is the right size in the wrong place passes a single-pixel check. */
static unsigned coverage(int box[4])
{
    unsigned char pixels[SIDE * SIDE * 4];
    glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    unsigned covered = 0;
    int minx = 1 << 20, miny = 1 << 20, maxx = -1, maxy = -1;
    for (int y = 0; y < SIDE; ++y) {
        for (int x = 0; x < SIDE; ++x) {
            if (pixels[(y * SIDE + x) * 4] < 200)
                continue;
            ++covered;
            if (x < minx) minx = x;
            if (y < miny) miny = y;
            if (x > maxx) maxx = x;
            if (y > maxy) maxy = y;
        }
    }
    box[0] = maxx < 0 ? -1 : minx;
    box[1] = maxy < 0 ? -1 : miny;
    box[2] = maxx;
    box[3] = maxy;
    return covered;
}

static int box_is(const int box[4], int x0, int y0, int x1, int y1)
{
    return box[0] == x0 && box[1] == y0 && box[2] == x1 && box[3] == y1;
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
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_viewport_array");
    GLint max_viewports = 0;
    glGetIntegerv(GL_MAX_VIEWPORTS, &max_viewports);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s viewport_array=%d max_viewports=%d\n",
            glGetString(GL_VERSION), have, max_viewports);
    result("count", have && max_viewports >= 16);

    GLuint colour = 0, depth = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenTextures(1, &depth);
    glBindTexture(GL_TEXTURE_2D, depth);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_DEPTH_COMPONENT24, SIDE, SIDE, 0,
                 GL_DEPTH_COMPONENT, GL_UNSIGNED_INT, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                           depth, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* Case 2: state at a non-zero index, with no drawing involved. This is
     * the half of the extension a one-viewport driver still has to store: st
     * sets every slot at context setup, so refusing them is not a missing
     * feature but a dead context. */
    {
        int ok = 0;
        if (have) {
            glViewportIndexedf(3, 5.0f, 7.0f, 11.0f, 13.0f);
            glScissorIndexed(3, 2, 4, 6, 8);
            glDepthRangeIndexed(3, 0.25, 0.75);
            GLfloat vp[4] = {0};
            GLint sc[4] = {0};
            GLdouble dr[2] = {0};
            glGetFloati_v(GL_VIEWPORT, 3, vp);
            glGetIntegeri_v(GL_SCISSOR_BOX, 3, sc);
            glGetDoublei_v(GL_DEPTH_RANGE, 3, dr);
            ok = vp[0] == 5.0f && vp[1] == 7.0f && vp[2] == 11.0f &&
                 vp[3] == 13.0f && sc[0] == 2 && sc[1] == 4 && sc[2] == 6 &&
                 sc[3] == 8 && dr[0] == 0.25 && dr[1] == 0.75;
            fprintf(stderr, "INFO roundtrip vp=%g,%g,%g,%g sc=%d,%d,%d,%d "
                    "dr=%g,%g\n", vp[0], vp[1], vp[2], vp[3], sc[0], sc[1],
                    sc[2], sc[3], dr[0], dr[1]);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO roundtrip ok=%d err=0x%04x\n", ok,
                (unsigned)error);
        result("roundtrip", have && ok && error == GL_NO_ERROR);
    }

    GLuint plain = have ? build(NULL) : 0;
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_CULL_FACE);

    /* Case 3: viewport zero through the indexed entry point. */
    {
        int box[4] = {-1, -1, -1, -1};
        unsigned covered = 0;
        if (plain) {
            glViewportIndexedf(0, 4.0f, 6.0f, 8.0f, 10.0f);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(plain);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            covered = coverage(box);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO indexed-zero covered=%u box=%d,%d..%d,%d "
                "want=80 4,6..11,15 err=0x%04x\n", covered, box[0], box[1],
                box[2], box[3], (unsigned)error);
        result("indexed-zero", plain && error == GL_NO_ERROR &&
                               covered == 80 && box_is(box, 4, 6, 11, 15));
    }

    /* Case 4: the scissor array, at index zero. */
    {
        int box[4] = {-1, -1, -1, -1};
        unsigned covered = 0;
        if (plain) {
            glViewportIndexedf(0, 0.0f, 0.0f, (float)SIDE, (float)SIDE);
            glScissorIndexed(0, 10, 12, 5, 6);
            glEnable(GL_SCISSOR_TEST);
            glClearColor(0, 0, 0, 1);
            glDisable(GL_SCISSOR_TEST);
            glClear(GL_COLOR_BUFFER_BIT);
            glEnable(GL_SCISSOR_TEST);
            glUseProgram(plain);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glDisable(GL_SCISSOR_TEST);
            covered = coverage(box);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO scissor-indexed covered=%u box=%d,%d..%d,%d "
                "want=30 10,12..14,17 err=0x%04x\n", covered, box[0], box[1],
                box[2], box[3], (unsigned)error);
        result("scissor-indexed", plain && error == GL_NO_ERROR &&
                                  covered == 30 &&
                                  box_is(box, 10, 12, 14, 17));
    }

    /* Case 5: the depth range array, at index zero. The quad is drawn at
     * z = 0 in clip space, which a range of 0.6..0.6 puts at 0.6 in the
     * depth buffer -- so a GL_LESS test against a buffer cleared to 0.5
     * rejects it, and against 0.7 accepts it. Two draws, so the answer
     * cannot come from the range being ignored. */
    {
        int reject_box[4], accept_box[4];
        unsigned rejected = 1, accepted = 0;
        if (plain) {
            glViewportIndexedf(0, 0.0f, 0.0f, (float)SIDE, (float)SIDE);
            glDepthRangeIndexed(0, 0.6, 0.6);
            glEnable(GL_DEPTH_TEST);
            glDepthFunc(GL_LESS);
            glUseProgram(plain);
            glClearColor(0, 0, 0, 1);
            glClearDepth(0.5);
            glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            rejected = coverage(reject_box);
            glClearDepth(0.7);
            glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            accepted = coverage(accept_box);
            glDisable(GL_DEPTH_TEST);
            glDepthRangeIndexed(0, 0.0, 1.0);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO depth-indexed rejected=%u accepted=%u "
                "want=0,%u err=0x%04x\n", rejected, accepted,
                (unsigned)(SIDE * SIDE), (unsigned)error);
        result("depth-indexed", plain && error == GL_NO_ERROR &&
                                rejected == 0 &&
                                accepted == (unsigned)(SIDE * SIDE));
    }

    /* Case 6: routing. A geometry shader writes gl_ViewportIndex = 1, and
     * viewport one is disjoint from viewport zero -- so where the pixels land
     * says which viewport was used, with no ambiguity. */
    {
        static const char *gs_src =
            "#version 330 core\n"
            "#extension GL_ARB_viewport_array : require\n"
            "layout(triangles) in;\n"
            "layout(triangle_strip, max_vertices = 3) out;\n"
            "void main() {\n"
            "  for (int i = 0; i < 3; ++i) {\n"
            "    gl_Position = gl_in[i].gl_Position;\n"
            "    gl_ViewportIndex = 1;\n"
            "    EmitVertex();\n"
            "  }\n"
            "  EndPrimitive();\n"
            "}\n";
        int box[4] = {-1, -1, -1, -1};
        unsigned covered = 0;
        GLuint routed = have ? build(gs_src) : 0;
        if (routed) {
            glViewportIndexedf(0, 0.0f, 0.0f, 4.0f, 4.0f);
            glViewportIndexedf(1, 16.0f, 20.0f, 8.0f, 6.0f);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(routed);
            /* Triangles, because a geometry shader takes assembled
             * primitives and `layout(triangles) in` needs three vertices. */
            glDrawArrays(GL_TRIANGLES, 0, 3);
            covered = coverage(box);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO gs-route covered=%u box=%d,%d..%d,%d "
                "want inside 16,20..23,25 err=0x%04x\n", covered, box[0],
                box[1], box[2], box[3], (unsigned)error);
        /* The vertex shader's first three vertices make a triangle covering
         * the lower-left half, so the exact count depends on the viewport's
         * shape; what matters is that every covered pixel is inside viewport
         * one and none is inside viewport zero. */
        result("gs-route", have && routed && error == GL_NO_ERROR &&
                           covered > 0 && box[0] >= 16 && box[1] >= 20 &&
                           box[2] <= 23 && box[3] <= 25);
        if (routed) glDeleteProgram(routed);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
