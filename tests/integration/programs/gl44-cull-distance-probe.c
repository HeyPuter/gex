/* ARB_cull_distance -- the one extension between this driver and GL 4.5.
 *
 * `gl_CullDistance` is not `gl_ClipDistance`, and the difference is the whole
 * of this probe. A clip distance cuts a primitive along the plane where it
 * reaches zero. A cull distance discards the primitive *entirely* and only
 * when **every** vertex is negative -- so a primitive with mixed signs is
 * drawn whole, including the parts where the distance is negative. Mapping
 * cull onto clip therefore looks right for the two easy cases and silently
 * eats a corner of the third, which is why that third case is here.
 *
 *   `all_negative_culled`  every vertex negative: nothing is drawn
 *   `mixed_kept`           one vertex negative, two positive: the triangle is
 *                          drawn *whole*, and the pixel sampled is the one
 *                          nearest the negative vertex -- the pixel a clip
 *                          implementation removes
 *   `all_positive_kept`    the control, without which a driver that culled
 *                          everything would pass the first case
 *
 * The sampled points are chosen so each case reads a pixel the other two
 * cannot explain: a cull that never happens fails the first, a cull that
 * always happens fails the third, and clipping instead of culling fails only
 * the second.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    for (GLint i = 0; i < count; ++i) {
        const GLubyte *one = glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (one && strcmp((const char *)one, want) == 0) return 1;
    }
    return 0;
}

#define SIDE 32

/* The cull distance comes in per vertex as an attribute, so one program
 * serves every case and only the vertex data changes. */
static GLuint build(void) {
    static const char *vs_src =
        "#version 450 core\n"
        "layout(location = 0) in vec2 position;\n"
        "layout(location = 1) in float cull;\n"
        "out float gl_CullDistance[1];\n"
        "void main() {\n"
        "  gl_Position = vec4(position, 0.0, 1.0);\n"
        "  gl_CullDistance[0] = cull;\n"
        "}\n";
    static const char *fs_src =
        "#version 450 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(0.0, 1.0, 0.0, 1.0); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO vs compile: %s\n", log);
        return 0;
    }
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    glGetShaderiv(fs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        return 0;
    }
    return prog;
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
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context =
        eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int have = have_extension("GL_ARB_cull_distance");
    GLint most = 0;
    glGetIntegerv(0x82F9 /* GL_MAX_CULL_DISTANCES */, &most);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s cull_distance=%d max=%d\n",
            glGetString(GL_VERSION), have, most);
    if (!have) {
        fprintf(stderr, "INFO GL_ARB_cull_distance absent; nothing to test\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }

    GLuint colour = 0, fbo = 0, vao = 0, buffer = 0;
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
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 3 * sizeof(float), NULL);
    glEnableVertexAttribArray(1);
    glVertexAttribPointer(1, 1, GL_FLOAT, GL_FALSE, 3 * sizeof(float),
                          (const void *)(2 * sizeof(float)));

    GLuint program = build();
    if (!program) {
        fprintf(stderr, "INFO the shader did not build\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    glUseProgram(program);

    /* One triangle covering the whole viewport, so every case samples the
     * same geometry and only the cull values differ. Vertex 0 is the bottom
     * left corner, which is where `mixed` puts its negative distance. */
    struct { const char *name; float cull[3]; int want_green;
             int px; int py; } tests[] = {
        /* Every vertex negative: the triangle goes away entirely. */
        {"all_negative_culled", {-1.0f, -1.0f, -1.0f}, 0, SIDE / 2, SIDE / 2},
        /* One vertex negative. GL keeps the whole triangle, so even the
         * corner *at* the negative vertex is drawn -- that pixel is what a
         * clip implementation removes and a cull implementation keeps. */
        {"mixed_kept", {-1.0f, 1.0f, 1.0f}, 1, 2, 2},
        /* The control. */
        {"all_positive_kept", {1.0f, 1.0f, 1.0f}, 1, SIDE / 2, SIDE / 2},
    };

    for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); ++i) {
        const float vertices[9] = {
            -1.0f, -1.0f, tests[i].cull[0],
             3.0f, -1.0f, tests[i].cull[1],
            -1.0f,  3.0f, tests[i].cull[2],
        };
        glBindBuffer(GL_ARRAY_BUFFER, buffer);
        glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices,
                     GL_STREAM_DRAW);
        glClearColor(1.0f, 0.0f, 0.0f, 1.0f);   /* red where nothing draws */
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        unsigned char px[4] = {0};
        glReadPixels(tests[i].px, tests[i].py, 1, 1, GL_RGBA,
                     GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        const int green = px[1] > 128 && px[0] < 128;
        const int red = px[0] > 128 && px[1] < 128;
        const int ok = error == GL_NO_ERROR &&
                       (tests[i].want_green ? green : red);
        fprintf(stderr, "INFO %s at %d,%d got=%u,%u,%u want=%s err=0x%04x\n",
                tests[i].name, tests[i].px, tests[i].py, px[0], px[1], px[2],
                tests[i].want_green ? "green" : "red", (unsigned)error);
        result(tests[i].name, ok);
    }

    /* A cull distance in the *upper* half of the merged clip/cull array.
     *
     * gl_ClipDistance and gl_CullDistance share one array of up to eight
     * components, spanning two varying slots. This driver decides cull on the
     * CPU from a capture of the vertex shader's outputs, and that capture
     * matched its slots by location -- but a shader with more than four
     * distances between the two declares a single variable at CLIP_DIST0 and
     * there is no variable at CLIP_DIST1 to find, so the upper four
     * components captured as zero. Zero does not cull, so gl_CullDistance
     * simply stopped working past the fourth component while the three cases
     * above, which use component zero, kept passing.
     *
     * Here the culling distance is component *five* of the merged array --
     * clip[0..2] then cull[0..4], and it is cull[1] that goes negative --
     * which lands in the second slot. Three clip distances are declared and
     * held positive so they cannot clip anything away themselves.
     */
    {
        static const char *vs_src =
            "#version 450 core\n"
            "layout(location = 0) in vec2 position;\n"
            "layout(location = 1) in float cull;\n"
            "out float gl_ClipDistance[3];\n"
            "out float gl_CullDistance[5];\n"
            "void main() {\n"
            "  gl_Position = vec4(position, 0.0, 1.0);\n"
            "  gl_ClipDistance[0] = 1.0;\n"
            "  gl_ClipDistance[1] = 1.0;\n"
            "  gl_ClipDistance[2] = 1.0;\n"
            "  gl_CullDistance[0] = 1.0;\n"
            "  gl_CullDistance[1] = cull;\n"
            "  gl_CullDistance[2] = 1.0;\n"
            "  gl_CullDistance[3] = 1.0;\n"
            "  gl_CullDistance[4] = 1.0;\n"
            "}\n";
        static const char *fs_src =
            "#version 450 core\n"
            "out vec4 colour;\n"
            "void main() { colour = vec4(0.0, 1.0, 0.0, 1.0); }\n";
        GLint most_clip = 0;
        glGetIntegerv(GL_MAX_CLIP_DISTANCES, &most_clip);
        while (glGetError() != GL_NO_ERROR) {}
        GLuint vs = glCreateShader(GL_VERTEX_SHADER);
        glShaderSource(vs, 1, &vs_src, NULL);
        glCompileShader(vs);
        GLint ok = 0;
        glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
        GLuint prog = 0;
        if (ok) {
            GLuint fs2 = glCreateShader(GL_FRAGMENT_SHADER);
            glShaderSource(fs2, 1, &fs_src, NULL);
            glCompileShader(fs2);
            prog = glCreateProgram();
            glAttachShader(prog, vs);
            glAttachShader(prog, fs2);
            glLinkProgram(prog);
            glGetProgramiv(prog, GL_LINK_STATUS, &ok);
            if (!ok) {
                char log[900] = {0};
                glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
                fprintf(stderr, "INFO upper_half link: %s\n", log);
                prog = 0;
            }
        } else {
            char log[900] = {0};
            glGetShaderInfoLog(vs, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO upper_half compile: %s\n", log);
        }
        if (!prog || most_clip < 3) {
            fprintf(stderr, "INFO upper_half unavailable (max_clip=%d)\n",
                    most_clip);
            result("cull_upper_half", 0);
        } else {
            glUseProgram(prog);
            const float vertices[9] = {
                -1.0f, -1.0f, -1.0f,
                 3.0f, -1.0f, -1.0f,
                -1.0f,  3.0f, -1.0f,
            };
            glBindBuffer(GL_ARRAY_BUFFER, buffer);
            glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices,
                         GL_STREAM_DRAW);
            glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            unsigned char px[4] = {0};
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA,
                         GL_UNSIGNED_BYTE, px);
            const GLenum error = glGetError();
            fprintf(stderr, "INFO cull_upper_half got=%u,%u,%u want=red "
                    "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
            /* Every vertex negative in cull[1]: the triangle goes away and
             * the clear colour stands. */
            result("cull_upper_half", error == GL_NO_ERROR &&
                                      px[0] > 128 && px[1] < 128);
            glUseProgram(program);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
