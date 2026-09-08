/* Switching a storage block's binding between draws in one frame.
 *
 * Two shapes, and GL treats them the same way: `glBindBufferBase` moving a
 * block to a *different buffer* between draws, and `glBindBufferRange` moving
 * it to a *different offset within one buffer*. Each draw must read what was
 * bound for it and nothing else.
 *
 * Written for KHR-GL45.shader_storage_buffer_object.advanced-switchBuffers,
 * which does both and which fails **in the browser only** -- "counted bad:
 * 1800" -- while passing natively. A browser-only failure cannot be a driver
 * arithmetic bug: the same driver code runs on both. It is the guest
 * marshalling of the bind call, or a bind group cached across a rebind, and
 * neither is visible to any native gate.
 *
 *   `rebind_buffer`   three draws, three *different* buffers on binding 0,
 *                     each holding a distinct value
 *   `rebind_range`    three draws, three different offsets into *one* buffer.
 *                     This is the half where an offset dropped in transit or
 *                     left out of a cache key shows up, and it is why the
 *                     values are chosen so that reading offset zero every
 *                     time is a *different* answer from reading correctly
 *   `last_wins`       after the loops, one more rebind and draw, to catch a
 *                     rebind that is only applied on the *next* draw rather
 *                     than this one -- an off-by-one in the state flush
 *   `rebind_vertex`   the same three rebinds with the block read in the
 *                     **vertex** stage instead of the fragment one. This is
 *                     the shape switchBuffers actually has -- its shader
 *                     reads the quad's *positions* out of the block, which is
 *                     why a stale binding leaves the quadrant black rather
 *                     than the wrong colour -- and the two stages bind
 *                     storage through different groups, so passing one says
 *                     nothing about the other
 *
 * Each draw reads its value into the colour target and is read back
 * immediately, so a wrong answer names which draw was wrong rather than
 * leaving one number for the whole sequence.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 8
#define STRIDE 256      /* comfortably above any offset alignment GL asks */

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* The same read, moved to the vertex stage and carried across flat. */
static GLuint build_vertex_read(void) {
    static const char *vs_src =
        "#version 430 core\n"
        "layout(std430, binding = 0) buffer Src { uint v[4]; } s;\n"
        "flat out uint seen;\n"
        "void main() {\n"
        "  seen = s.v[0];\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 430 core\n"
        "flat in uint seen;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(float(seen) / 255.0, 0, 0, 1); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vertex-read vs compile: %s\n", log);
        return 0;
    }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vertex-read link: %s\n", log);
        return 0;
    }
    return prog;
}

static GLuint build(void) {
    static const char *vs_src =
        "#version 430 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 430 core\n"
        "layout(std430, binding = 0) buffer Src { uint v[4]; } s;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(float(s.v[0]) / 255.0, 0, 0, 1); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLint ok = 0;
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(fs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO fs compile: %s\n", log);
        return 0;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) return 0;
    return prog;
}

static unsigned draw_and_read(void) {
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {0};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    return px[0];
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
    const EGLint surface_attributes[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                         EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    GLuint prog = build();
    if (!prog) {
        result("rebind_buffer", 0);
        result("rebind_range", 0);
        result("last_wins", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 0;
    }
    glUseProgram(prog);

    /* Three separate buffers, values 30, 60, 90. */
    GLuint bufs[3] = {0};
    glGenBuffers(3, bufs);
    for (int i = 0; i < 3; ++i) {
        const unsigned data[4] = {(unsigned)(30 * (i + 1)), 0, 0, 0};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, bufs[i]);
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(data), data,
                     GL_STATIC_DRAW);
    }
    int buffer_ok = 1;
    for (int i = 0; i < 3; ++i) {
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, bufs[i]);
        const unsigned got = draw_and_read();
        const unsigned want = 30u * (i + 1);
        fprintf(stderr, "INFO buffer %d got=%u want=%u\n", i, got, want);
        if (got != want) buffer_ok = 0;
    }
    result("rebind_buffer", buffer_ok);

    /* One buffer, three records a STRIDE apart, values 40, 80, 120. Reading
     * offset zero every time would give 40,40,40 -- a different answer. */
    GLuint one = 0;
    glGenBuffers(1, &one);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, one);
    glBufferData(GL_SHADER_STORAGE_BUFFER, 3 * STRIDE, NULL, GL_STATIC_DRAW);
    for (int i = 0; i < 3; ++i) {
        const unsigned data[4] = {(unsigned)(40 * (i + 1)), 0, 0, 0};
        glBufferSubData(GL_SHADER_STORAGE_BUFFER, (GLintptr)i * STRIDE,
                        sizeof(data), data);
    }
    int range_ok = 1;
    for (int i = 0; i < 3; ++i) {
        glBindBufferRange(GL_SHADER_STORAGE_BUFFER, 0, one,
                          (GLintptr)i * STRIDE, 16);
        const unsigned got = draw_and_read();
        const unsigned want = 40u * (i + 1);
        fprintf(stderr, "INFO range %d got=%u want=%u\n", i, got, want);
        if (got != want) range_ok = 0;
    }
    result("rebind_range", range_ok);

    /* And one final rebind, to catch a state flush that applies a rebind to
     * the draw *after* the one it was issued for. */
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, bufs[0]);
    const unsigned last = draw_and_read();
    fprintf(stderr, "INFO last got=%u want=30 err=0x%04x\n", last,
            (unsigned)glGetError());
    result("last_wins", last == 30);

    /* And the shape switchBuffers has: the block read in the vertex stage.
     * The two stages bind storage through different groups, so the cases
     * above passing says nothing about this one. */
    GLuint vprog = build_vertex_read();
    int vertex_ok = 0;
    if (vprog) {
        glUseProgram(vprog);
        vertex_ok = 1;
        for (int i = 0; i < 3; ++i) {
            glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, bufs[i]);
            const unsigned got = draw_and_read();
            const unsigned want = 30u * (i + 1);
            fprintf(stderr, "INFO vertex buffer %d got=%u want=%u\n", i, got,
                    want);
            if (got != want) vertex_ok = 0;
        }
        for (int i = 0; i < 3; ++i) {
            glBindBufferRange(GL_SHADER_STORAGE_BUFFER, 0, one,
                              (GLintptr)i * STRIDE, 16);
            const unsigned got = draw_and_read();
            const unsigned want = 40u * (i + 1);
            fprintf(stderr, "INFO vertex range %d got=%u want=%u\n", i, got,
                    want);
            if (got != want) vertex_ok = 0;
        }
    }
    result("rebind_vertex", vertex_ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
