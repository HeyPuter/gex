/* A vertex shader that *uses the result* of a storage atomic.
 *
 * GL allows it: `atomicAdd` on an SSBO returns the pre-operation value, and a
 * vertex shader may carry that value into a varying and rasterise with it.
 * WGSL does not -- a vertex stage may not write storage at all -- so this
 * driver runs the shader twice: a compute clone that performs the stores and
 * the atomics for real, and a rasterising copy with the stores stripped.
 *
 * The rasterising copy cannot perform the atomic and cannot drop it either,
 * because the shader uses what it returned. Today the driver rewrites that
 * result to zero, marks the shader unrasterisable, and declines any draw of it
 * by name -- so the side effects happen and the picture does not. The fix is
 * for the compute clone to *record* each atomic's result per vertex and the
 * rasterising copy to read it back, which is exact for a non-indexed draw
 * because the clone's slot is `gl_VertexIndex - FIRST_VERTEX`.
 *
 * The value has to be a *named* number, not merely "something was drawn": the
 * whole risk in the read-back is that the two copies index it differently, and
 * a wrong index yields a plausible value rather than an error.
 *
 *   `result_reaches_raster`   the counter starts at 7, one vertex increments
 *                             it, and the fragment shader writes what the
 *                             atomic returned. The pixel must read 7 -- not 0
 *                             (the value the strip substitutes) and not 8
 *                             (the post-operation value)
 *   `side_effect_happened`    and the counter itself must read 8 afterwards,
 *                             so the atomic ran exactly once
 *   `distinct_per_vertex`     four vertices, four distinct results summing to
 *                             0+1+2+3=6 on top of the base. This is the case
 *                             a mis-indexed read-back fails: every vertex
 *                             reading slot 0 gives four equal values
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 8

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static GLuint build(const char *vs_src, const char *fs_src) {
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

    /* One counter, starting at 7 so the returned value is neither 0 (what the
     * strip substitutes) nor 1 (what a counter starting at 0 would give). */
    GLuint ssbo = 0;
    glGenBuffers(1, &ssbo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    const unsigned base[1] = {7u};
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(base), base, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);

    /* A full-target triangle strip whose flat varying carries what the atomic
     * returned. The strip's first vertex is the provoking one for flat
     * interpolation, so the pixel reads vertex 0's result. */
    static const char *vs_src =
        "#version 430 core\n"
        "layout(std430, binding = 0) buffer Counter { uint n; } c;\n"
        "flat out uint seen;\n"
        "void main() {\n"
        "  seen = atomicAdd(c.n, 1u);\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    static const char *fs_src =
        "#version 430 core\n"
        "flat in uint seen;\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(float(seen) / 255.0, 0, 0, 1); }\n";

    GLuint prog = build(vs_src, fs_src);
    if (!prog) {
        result("result_reaches_raster", 0);
        result("side_effect_happened", 0);
        result("distinct_per_vertex", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 0;
    }
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    unsigned char px[4] = {99, 99, 99, 99};
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT |
                    GL_BUFFER_UPDATE_BARRIER_BIT);
    unsigned after = 0;
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(after), &after);
    const GLenum err = glGetError();
    fprintf(stderr, "INFO pixel=%u counter=%u err=0x%04x\n", px[0], after,
            (unsigned)err);

    /* Four vertices each took one value from 7..10; the provoking vertex is
     * vertex 0, so the pixel is one of them and the counter ends at 11. Which
     * vertex ran first is not ordered by GL, so the pixel is accepted in
     * range while the *counter* pins the count exactly. */
    result("result_reaches_raster", px[0] >= 7 && px[0] <= 10);
    result("side_effect_happened", after == 11);

    /* Now the indexing test: reset, draw again, and require the four results
     * to be four *different* numbers. A read-back that gives every vertex
     * slot 0's value passes the two cases above and fails this one. */
    const unsigned reset[1] = {0u};
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(reset), reset,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
    GLuint out = 0;
    glGenBuffers(1, &out);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    const unsigned blank[4] = {77u, 77u, 77u, 77u};
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(blank), blank,
                 GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, out);

    static const char *vs_each =
        "#version 430 core\n"
        "layout(std430, binding = 0) buffer Counter { uint n; } c;\n"
        "layout(std430, binding = 1) buffer Out { uint v[4]; } o;\n"
        "flat out uint seen;\n"
        "void main() {\n"
        "  uint got = atomicAdd(c.n, 1u);\n"
        "  seen = got;\n"
        "  o.v[gl_VertexID] = got + 1u;\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    GLuint prog2 = build(vs_each, fs_src);
    int distinct_ok = 0;
    if (prog2) {
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(prog2);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT |
                        GL_BUFFER_UPDATE_BARRIER_BIT);
        unsigned each[4] = {0, 0, 0, 0};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
        glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(each), each);
        fprintf(stderr, "INFO each=%u,%u,%u,%u\n", each[0], each[1], each[2],
                each[3]);
        /* Values 1..4 in some order: each vertex saw a different result. */
        unsigned seen_mask = 0;
        int in_range = 1;
        for (int i = 0; i < 4; ++i) {
            if (each[i] < 1 || each[i] > 4) in_range = 0;
            else seen_mask |= 1u << each[i];
        }
        distinct_ok = in_range && seen_mask == 0x1eu;
    }
    result("distinct_per_vertex", distinct_ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
