/* gl_PrimitiveID in a fragment shader, with no geometry or tessellation stage.
 *
 * WGSL has no primitive id, so this reaches Tint as builtin 7 and the whole
 * module is refused -- which arrives at the application as a **link error**,
 * so a program using it does not run at all. That is the severe shape: not a
 * wrong pixel, an application that will not start.
 *
 * Every vertex of a primitive can compute the id itself from its vertex index
 * once the draw is a *list* topology and de-indexed, and for a list the
 * integer division makes all of a primitive's vertices agree -- so the flat
 * provoking-vertex convention, which this driver otherwise has to rewrite
 * index lists for, does not enter into it.
 *
 * Each case draws primitives whose ids differ, into distinct pixels, so a
 * driver that returns a constant (zero, say) passes nothing:
 *
 *   triangles       three triangles, ids 0/1/2, one per horizontal band
 *   triangles_idx   the same drawn indexed, where the vertex index is the
 *                   *index value* rather than a sequence number
 *   strip           a triangle strip, where a primitive's vertices do not
 *                   share a first vertex, so the flat convention matters
 *
 * The list geometry is three wide, non-overlapping bands rather than three
 * thin columns. The first attempt used thin right triangles and sampled their
 * narrow end: llvmpipe read zeros, because the sample points were outside the
 * triangles. A probe whose *reference* fails is measuring its own geometry,
 * not the driver.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define SIDE 24

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
/* The id goes out as a colour so it can be read back exactly. */
static const char *fs_src =
    "#version 330 core\n"
    "out uvec4 colour;\n"
    "void main() { colour = uvec4(uint(gl_PrimitiveID) + 1u, 0u, 0u, 1u); }\n";

/* Three wide triangles, one per horizontal band and none overlapping: each
 * spans well past both edges at its base, so a sample at the middle column a
 * little above the base is inside it with room to spare. */
static const float tri_pos[9][2] = {
    {-2.0f, -0.95f}, {2.0f, -0.95f}, {0.0f, -0.40f},
    {-2.0f, -0.28f}, {2.0f, -0.28f}, {0.0f,  0.28f},
    {-2.0f,  0.40f}, {2.0f,  0.40f}, {0.0f,  0.95f},
};
/* A strip of three triangles: vertices 0..4 give primitives (0,1,2), (1,2,3)
 * and (2,3,4). Two vertical bands, so primitive 0 is the lower-left half of
 * the first, primitive 1 the upper-right of it, and primitive 2 the lower part
 * of the second -- each with a sample point of its own below. */
static const float strip_pos[5][2] = {
    {-1.0f, -1.0f}, {-1.0f, 1.0f}, {0.0f, -1.0f}, {0.0f, 1.0f}, {1.0f, -1.0f},
};

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

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(prog, sizeof log, NULL, log);
        fprintf(stderr, "INFO link failed: %s\n", log);
        /* Named as its own case: a program that will not link is a different
         * failure from one that links and reads the wrong id, and only one of
         * them stops an application from starting. */
        result("links", 0);
        result("triangles", 0);
        result("triangles_idx", 0);
        result("strip", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 0;
    }
    result("links", 1);
    glUseProgram(prog);

    GLuint vao = 0, vbo = 0, ibo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glGenBuffers(1, &ibo);

    GLuint tex = 0, fbo = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, SIDE, SIDE, 0, GL_RED_INTEGER,
                 GL_UNSIGNED_INT, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 1;
    }
    glViewport(0, 0, SIDE, SIDE);
    glDisable(GL_DEPTH_TEST);

    /* The id at the centre of each third, plus one, so zero means nothing was
     * drawn there and can be told from primitive zero. */
    static const GLushort indices[9] = {0, 1, 2, 3, 4, 5, 6, 7, 8};
    for (unsigned run = 0; run < 3; ++run) {
        const int strip = run == 2;
        const float (*pos)[2] = strip ? strip_pos : tri_pos;
        const unsigned verts = strip ? 5u : 9u;
        glBindBuffer(GL_ARRAY_BUFFER, vbo);
        glBufferData(GL_ARRAY_BUFFER,
                     (GLsizeiptr)(verts * 2 * sizeof(float)), pos,
                     GL_STATIC_DRAW);
        glEnableVertexAttribArray(0);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

        const GLuint zero[4] = {0, 0, 0, 0};
        glClearBufferuiv(GL_COLOR, 0, zero);
        if (run == 1) {
            glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
            glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof indices, indices,
                         GL_STATIC_DRAW);
            glDrawElements(GL_TRIANGLES, 9, GL_UNSIGNED_SHORT, NULL);
        } else if (strip) {
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 5);
        } else {
            glDrawArrays(GL_TRIANGLES, 0, 9);
        }

        GLuint got[3] = {0, 0, 0};
        /* One point per primitive. The bands are horizontal for the lists, so
         * the middle column at three heights; the strip needs a point in each
         * of its three differently-shaped pieces. */
        const int list_x[3] = {SIDE / 2, SIDE / 2, SIDE / 2};
        const int list_y[3] = {SIDE / 8, SIDE / 2, 7 * SIDE / 8};
        const int strip_x[3] = {SIDE / 8, 3 * SIDE / 8, 5 * SIDE / 8};
        const int strip_y[3] = {SIDE / 8, 7 * SIDE / 8, SIDE / 8};
        for (unsigned i = 0; i < 3; ++i)
            glReadPixels(strip ? strip_x[i] : list_x[i],
                         strip ? strip_y[i] : list_y[i], 1, 1,
                         GL_RED_INTEGER, GL_UNSIGNED_INT, &got[i]);
        static const char *names[3] = {"triangles", "triangles_idx", "strip"};
        fprintf(stderr, "INFO %s ids=%u,%u,%u want 1,2,3 err=0x%04x\n",
                names[run], got[0], got[1], got[2], (unsigned)glGetError());
        result(names[run], got[0] == 1 && got[1] == 2 && got[2] == 3);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
