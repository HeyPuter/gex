/* glBeginConditionalRender, both senses, and glTextureBarrier.
 *
 * The driver set caps->conditional_render -- so Mesa advertised
 * ARB_conditional_render, which is GL *3.0* -- while render_condition failed
 * the context for any query, on a comment that queries "do not exist here".
 * They had existed since occlusion queries landed. So an application that
 * used a feature the driver claimed got GL_OUT_OF_MEMORY from every later
 * call, and nothing tested it.
 *
 * The convention is worth stating because it is not obvious: st passes
 * `condition` true for GL's *_INVERTED modes, and a driver tests the
 * *negation* of "result is non-zero" against it. Getting it backwards
 * inverts every conditional draw, which is why both senses are covered here
 * against a query that passes and one that does not.
 *
 * ARB_conditional_render_inverted and NV_texture_barrier are two of the five
 * extensions GL 4.5 still wants.
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

#define SIDE 8

/* Not in Mesa's non-glvnd libGL export list; see the note in
 * gl44-texture-sampler-probe.c. */
static void (*resolved_texture_barrier)(void);

static int have_extension(const char *want) {
    GLint count = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &count);
    while (glGetError() != GL_NO_ERROR) {}
    for (GLint i = 0; i < count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS, (GLuint)i);
        if (name && strcmp(name, want) == 0) return 1;
    }
    return 0;
}

static GLuint build(void) {
    static const char *vs =
        "#version 330 core\n"
        /* The clip-space z, so the depth-test cases below can put the quad
         * in front of or behind the cleared depth. Zero for every other
         * case, which is what it was before. */
        "uniform float zdepth;\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, zdepth, 1.0);\n"
        "}\n";
    static const char *fs =
        "#version 330 core\n"
        "uniform vec4 tint;\n"
        "out vec4 colour;\n"
        "void main() { colour = tint; }\n";
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs, NULL); glCompileShader(v);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs, NULL); glCompileShader(f);
    GLuint p = glCreateProgram();
    glAttachShader(p, v); glAttachShader(p, f); glLinkProgram(p);
    GLint ok = 0;
    glGetProgramiv(p, GL_LINK_STATUS, &ok);
    return ok ? p : 0;
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
        /* 4.3, not 3.3: the dispatch case below needs compute. Conditional
         * render itself is GL 3.0 and the draw cases would run either way. */
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                         EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    const int inverted = have_extension("GL_ARB_conditional_render_inverted");
    const int barrier = have_extension("GL_NV_texture_barrier") ||
                        have_extension("GL_ARB_texture_barrier");
    resolved_texture_barrier =
        (void (*)(void))eglGetProcAddress("glTextureBarrier");
    if (!resolved_texture_barrier)
        resolved_texture_barrier =
            (void (*)(void))eglGetProcAddress("glTextureBarrierNV");
    fprintf(stderr, "INFO version=%s inverted=%d barrier=%d entry=%d\n",
            glGetString(GL_VERSION), inverted, barrier,
            resolved_texture_barrier != NULL);

    GLuint colour = 0, fbo = 0, vao = 0, zbuf = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    /* A depth attachment, for the ANY_SAMPLES_PASSED cases at the end. The
     * earlier cases never enable the depth test, so it changes nothing for
     * them. */
    glGenRenderbuffers(1, &zbuf);
    glBindRenderbuffer(GL_RENDERBUFFER, zbuf);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH_COMPONENT24, SIDE, SIDE);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
                              GL_RENDERBUFFER, zbuf);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    GLuint program = build();
    if (!program) return 2;
    glUseProgram(program);
    const GLint tint = glGetUniformLocation(program, "tint");
    const GLint depth_at = glGetUniformLocation(program, "zdepth");

    /* A query that counts samples, run either over a full-screen draw (it
     * passes) or with an empty scissor (it does not). */
    struct { const char *name; int query_passes; GLenum mode; int want_green; }
    tests[] = {
        {"passes",          1, GL_QUERY_WAIT,          1},
        {"fails",           0, GL_QUERY_WAIT,          0},
        {"inverted-passes", 0, GL_QUERY_WAIT_INVERTED, 1},
        {"inverted-fails",  1, GL_QUERY_WAIT_INVERTED, 0},
    };

    for (unsigned t = 0; t < sizeof(tests) / sizeof(tests[0]); ++t) {
        if (tests[t].mode == GL_QUERY_WAIT_INVERTED && !inverted) {
            fprintf(stderr, "INFO %s skipped (no inverted)\n", tests[t].name);
            continue;
        }
        GLuint query = 0;
        glGenQueries(1, &query);
        /* Fill the predicate. The draw itself goes to the same target and is
         * overwritten by the clear below, so only the sample count matters. */
        glBeginQuery(GL_SAMPLES_PASSED, query);
        if (!tests[t].query_passes) {
            glEnable(GL_SCISSOR_TEST);
            glScissor(0, 0, 0, 0);
        }
        glUniform4f(tint, 1, 0, 0, 1);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glDisable(GL_SCISSOR_TEST);
        glEndQuery(GL_SAMPLES_PASSED);

        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glBeginConditionalRender(query, tests[t].mode);
        glUniform4f(tint, 0, 1, 0, 1);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glEndConditionalRender();

        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        const GLenum error = glGetError();
        const int green = px[1] > 200 && px[0] < 60;
        fprintf(stderr, "INFO %s got=%u,%u,%u want_green=%d err=0x%04x\n",
                tests[t].name, px[0], px[1], px[2], tests[t].want_green,
                (unsigned)error);
        result(tests[t].name, error == GL_NO_ERROR &&
                              green == tests[t].want_green);
        glDeleteQueries(1, &query);
    }

    /* glTextureBarrier: write the texture, barrier, then sample it into a
     * second target. Without the barrier this is undefined; with it the write
     * must be visible. */
    if (barrier && resolved_texture_barrier) {
        glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        resolved_texture_barrier();
        GLenum error = glGetError();
        unsigned char px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        if (error == GL_NO_ERROR) error = glGetError();
        fprintf(stderr, "INFO barrier got=%u,%u,%u want=64,128,191 err=0x%04x\n",
                px[0], px[1], px[2], (unsigned)error);
        result("texture-barrier",
               error == GL_NO_ERROR && px[0] >= 62 && px[0] <= 66 &&
               px[1] >= 126 && px[1] <= 130 && px[2] >= 189 && px[2] <= 193);
    } else {
        fprintf(stderr, "INFO texture-barrier skipped (ext=%d entry=%d)\n",
                barrier, resolved_texture_barrier != NULL);
    }

    /* glClear and glDispatchCompute are governed by conditional render too --
     * GL lists both among the commands glBeginConditionalRender discards --
     * and neither asked. The driver consulted the predicate in its draw and
     * blit paths and nowhere else, so a clear or a dispatch inside a failed
     * conditional happened anyway.
     *
     * Both are checked in *both* senses. A case that only tests the skip
     * passes on a driver that never performs the command at all, which is a
     * different defect wearing the same result. */
    {
        GLuint query = 0;
        for (unsigned pass = 0; pass < 2; ++pass) {
            const int predicate_passes = pass == 1;
            glGenQueries(1, &query);
            glBeginQuery(GL_SAMPLES_PASSED, query);
            if (!predicate_passes) {
                glEnable(GL_SCISSOR_TEST);
                glScissor(0, 0, 0, 0);
            }
            glUniform4f(tint, 1, 0, 0, 1);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glDisable(GL_SCISSOR_TEST);
            glEndQuery(GL_SAMPLES_PASSED);

            /* Red outside the conditional; the clear inside asks for green. */
            glClearColor(1, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glBeginConditionalRender(query, GL_QUERY_WAIT);
            glClearColor(0, 1, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glEndConditionalRender();
            glFinish();

            unsigned char px[4] = {0};
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
            const GLenum error = glGetError();
            const int green = px[1] > 200 && px[0] < 60;
            fprintf(stderr, "INFO clear-%s got=%u,%u,%u want_green=%d err=0x%04x\n",
                    predicate_passes ? "passes" : "fails", px[0], px[1], px[2],
                    predicate_passes, (unsigned)error);
            result(predicate_passes ? "clear-passes" : "clear-fails",
                   error == GL_NO_ERROR && green == predicate_passes);
            glDeleteQueries(1, &query);
        }
    }

    {
        static const char *cs_src =
            "#version 430 core\n"
            "layout(local_size_x = 1) in;\n"
            "layout(std430, binding = 0) buffer Out { uint v; } o;\n"
            "void main() { o.v = 0xabcdu; }\n";
        GLuint cs = glCreateShader(GL_COMPUTE_SHADER);
        glShaderSource(cs, 1, &cs_src, NULL);
        glCompileShader(cs);
        GLint ok = 0;
        glGetShaderiv(cs, GL_COMPILE_STATUS, &ok);
        GLuint cprog = 0;
        if (ok) {
            cprog = glCreateProgram();
            glAttachShader(cprog, cs);
            glLinkProgram(cprog);
            glGetProgramiv(cprog, GL_LINK_STATUS, &ok);
        }
        if (!ok) {
            char log[600] = {0};
            glGetShaderInfoLog(cs, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO dispatch skipped (compute unavailable): %s\n",
                    log);
        } else {
            GLuint ssbo = 0;
            glGenBuffers(1, &ssbo);
            for (unsigned pass = 0; pass < 2; ++pass) {
                const int predicate_passes = pass == 1;
                GLuint query = 0;
                glGenQueries(1, &query);
                glUseProgram(program);
                glBeginQuery(GL_SAMPLES_PASSED, query);
                if (!predicate_passes) {
                    glEnable(GL_SCISSOR_TEST);
                    glScissor(0, 0, 0, 0);
                }
                glUniform4f(tint, 1, 0, 0, 1);
                glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
                glDisable(GL_SCISSOR_TEST);
                glEndQuery(GL_SAMPLES_PASSED);

                const GLuint seed = 0u;
                glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
                glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof seed, &seed,
                             GL_DYNAMIC_COPY);
                glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
                glUseProgram(cprog);
                glBeginConditionalRender(query, GL_QUERY_WAIT);
                glDispatchCompute(1, 1, 1);
                glEndConditionalRender();
                glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
                glFinish();

                GLuint got = 0xffffffffu;
                glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof got,
                                   &got);
                const GLenum error = glGetError();
                const GLuint want = predicate_passes ? 0xabcdu : 0u;
                fprintf(stderr,
                        "INFO dispatch-%s got=0x%04x want=0x%04x err=0x%04x\n",
                        predicate_passes ? "passes" : "fails", got, want,
                        (unsigned)error);
                result(predicate_passes ? "dispatch-passes" : "dispatch-fails",
                       error == GL_NO_ERROR && got == want);
                glDeleteQueries(1, &query);
                glUseProgram(program);
            }
        }
    }

    /* GL_ANY_SAMPLES_PASSED with the *depth test* as the discriminator, which
     * is the shape KHR-GL45.compute_shader.conditional-dispatching uses and
     * the one the cases above do not: they make a query fail with an empty
     * scissor, and a scissor rejects fragments before the depth test ever
     * runs.
     *
     * That case still fails with conditional render working, and the driver's
     * own GEX_WGPU_CONDRENDER_LOG says why -- *both* of its queries come back
     * zero, so the quad that should pass the depth test recorded no samples
     * either. Either the predicate query type or the depth test is wrong, and
     * these two cases separate them from everything already covered. */
    {
        GLuint query = 0;
        glEnable(GL_DEPTH_TEST);
        glDepthFunc(GL_LESS);
        for (unsigned pass = 0; pass < 2; ++pass) {
            const int should_pass = pass == 1;
            /* Depth cleared to 0.5; the quad is drawn nearer or further. */
            glClearDepth(0.5);
            glClearColor(0, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
            glGenQueries(1, &query);
            glBeginQuery(GL_ANY_SAMPLES_PASSED, query);
            glUniform4f(tint, 1, 0, 0, 1);
            glUniform1f(depth_at, should_pass ? -0.5f : 0.5f);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glEndQuery(GL_ANY_SAMPLES_PASSED);
            glUniform1f(depth_at, 0.0f);

            GLuint any = 0xffffu;
            glGetQueryObjectuiv(query, GL_QUERY_RESULT, &any);
            const GLenum error = glGetError();
            fprintf(stderr,
                    "INFO any-samples-%s got=%u want=%u err=0x%04x\n",
                    should_pass ? "visible" : "occluded", any,
                    (unsigned)should_pass, (unsigned)error);
            result(should_pass ? "any-samples-visible" : "any-samples-occluded",
                   error == GL_NO_ERROR && (any != 0) == should_pass);
            glDeleteQueries(1, &query);
        }
        glDisable(GL_DEPTH_TEST);
    }

    /* Two occlusion queries outstanding at once, which is what
     * KHR-GL45.compute_shader.conditional-dispatching actually does and what
     * every case above avoids: each of those reads its result before starting
     * the next query, so only ever one query exists at a time.
     *
     * The CTS begins a query, draws a quad that passes the depth test, ends
     * it, begins a second, draws one that fails, ends it -- and only *then*
     * reads the first. A driver holding one shared occlusion slot and
     * resolving it lazily gives the first query whatever the second left
     * there, so both come back zero and the conditional render that follows
     * discards work it should have run.
     *
     * The two reads are deliberately after both queries have ended, and the
     * expected answers differ (visible then occluded), so a driver that
     * returns the same value for both fails whichever way that value goes. */
    {
        GLuint pair[2] = {0, 0};
        glEnable(GL_DEPTH_TEST);
        glDepthFunc(GL_LESS);
        glDepthMask(GL_TRUE);
        glClearDepth(0.5);
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
        glGenQueries(2, pair);

        /* Nearer than the cleared depth: passes, and writes -0.5. */
        glBeginQuery(GL_ANY_SAMPLES_PASSED, pair[0]);
        glUniform4f(tint, 1, 0, 0, 1);
        glUniform1f(depth_at, -0.5f);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glEndQuery(GL_ANY_SAMPLES_PASSED);

        /* Behind what the first draw just wrote: no sample survives. */
        glBeginQuery(GL_ANY_SAMPLES_PASSED, pair[1]);
        glUniform1f(depth_at, 0.5f);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glEndQuery(GL_ANY_SAMPLES_PASSED);
        glUniform1f(depth_at, 0.0f);

        GLuint first = 0xffffu, second = 0xffffu;
        glGetQueryObjectuiv(pair[0], GL_QUERY_RESULT, &first);
        glGetQueryObjectuiv(pair[1], GL_QUERY_RESULT, &second);
        const GLenum error = glGetError();
        fprintf(stderr,
                "INFO two-queries-pending first=%u (want nonzero) "
                "second=%u (want 0) err=0x%04x\n", first, second,
                (unsigned)error);
        result("two-queries-pending",
               error == GL_NO_ERROR && first != 0 && second == 0);
        glDeleteQueries(2, pair);
        glDisable(GL_DEPTH_TEST);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
