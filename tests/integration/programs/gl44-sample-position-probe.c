/* gl_SamplePosition, and glGetMultisamplefv agreeing with it.
 *
 * GL gives a fragment shader running per sample the position of the sample it
 * is running for, in the pixel's own [0,1] square, and promises that
 * `glGetMultisamplefv(GL_SAMPLE_POSITION, i)` reports the same point. Custom
 * multisample resolves and temporal jitter are written against exactly that
 * pair.
 *
 * This driver had neither half. `get_sample_position` was unimplemented, so
 * Mesa answered (0.5, 0.5) for every sample of a 4x target -- true of no
 * sample but the single-sampled one; and gl_SamplePosition reached Tint as
 * SPIR-V builtin 19, which it does not know, so the shader was *rejected* and
 * the program did not run at all.
 *
 * WebGPU does not let a pipeline place its samples, so they are the standard
 * locations Vulkan and D3D both specify, and the position is a lookup on
 * gl_SampleID -- which does reach WGSL, as @builtin(sample_index). Both
 * halves now read one table, which is what makes them agree.
 *
 *   `compiles`   the shader builds and links, which is the case that used to
 *                fail outright
 *   `query`      glGetMultisamplefv reports four *distinct* in-range points,
 *                not (0.5, 0.5) four times
 *   `agrees`     what the shader reads for sample i equals what the query
 *                reports for sample i -- the promise GL actually makes, and
 *                one a driver answering 0.5 for both halves would also pass,
 *                which is why `query` is separate
 *   `covers`     a triangle whose edge crosses the pixel covers *some* of its
 *                samples and not others, so the positions describe where the
 *                samples really are rather than four numbers agreeing with
 *                each other
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 4
#define SAMPLES 4

static GLuint build(const char *fs_src) {
    static const char *vs_src =
        "#version 400 core\n"
        "layout(location = 0) in vec2 at;\n"
        "void main() { gl_Position = vec4(at, 0.0, 1.0); }\n";
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
        fprintf(stderr, "INFO fragment compile: %s\n", log);
        return 0;
    }
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
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    /* A 4x multisample colour target, and a single-sampled one to resolve
     * the per-sample answers into where they can be read back. */
    GLuint ms = 0, fbo = 0, vao = 0, buffer = 0;
    glGenTextures(1, &ms);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms);
    /* 16F, not 32F: the positions are eighths, which 16F carries exactly,
     * and a 32F colour target is not renderable everywhere. */
    glTexImage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, SAMPLES, GL_RGBA16F,
                            SIDE, SIDE, GL_TRUE);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D_MULTISAMPLE, ms, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "INFO no 4x multisample target\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }
    GLint got_samples = 0;
    glGetIntegerv(GL_SAMPLES, &got_samples);
    fprintf(stderr, "INFO samples=%d\n", got_samples);
    if (got_samples != SAMPLES) {
        fprintf(stderr, "INFO not a 4x target; nothing to compare\n");
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 0;
    }
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 2 * sizeof(float), NULL);

    /* What the API says the sample positions are. */
    float queried[SAMPLES][2];
    for (int i = 0; i < SAMPLES; ++i) {
        queried[i][0] = queried[i][1] = -1.0f;
        glGetMultisamplefv(GL_SAMPLE_POSITION, (GLuint)i, queried[i]);
    }
    GLenum error = glGetError();
    int in_range = 1, distinct = 1;
    for (int i = 0; i < SAMPLES; ++i) {
        if (!(queried[i][0] >= 0.0f && queried[i][0] <= 1.0f &&
              queried[i][1] >= 0.0f && queried[i][1] <= 1.0f))
            in_range = 0;
        for (int j = i + 1; j < SAMPLES; ++j)
            if (fabsf(queried[i][0] - queried[j][0]) < 1e-4f &&
                fabsf(queried[i][1] - queried[j][1]) < 1e-4f)
                distinct = 0;
    }
    fprintf(stderr, "INFO query (%.3f,%.3f) (%.3f,%.3f) (%.3f,%.3f) "
            "(%.3f,%.3f) err=0x%04x\n",
            queried[0][0], queried[0][1], queried[1][0], queried[1][1],
            queried[2][0], queried[2][1], queried[3][0], queried[3][1],
            (unsigned)error);
    result("query", error == GL_NO_ERROR && in_range && distinct);

    /* Each sample writes its own gl_SamplePosition. Sample shading is forced
     * so the shader runs once per sample rather than once per pixel. */
    GLuint program = build(
        "#version 400 core\n"
        "out vec4 colour;\n"
        /* The constant in z says the draw happened at all, so a zero
         * position is not confused with a target nothing wrote. */
        "void main() { colour = vec4(gl_SamplePosition, 0.75, 1.0); }\n");
    result("compiles", program != 0);
    if (!program) {
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    glEnable(GL_SAMPLE_SHADING);
    glMinSampleShading(1.0f);
    const float full[8] = {
        -1.0f, -1.0f,  3.0f, -1.0f,  -1.0f, 3.0f,  0.0f, 0.0f,
    };
    glBufferData(GL_ARRAY_BUFFER, sizeof(full), full, GL_STREAM_DRAW);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(program);
    glDrawArrays(GL_TRIANGLES, 0, 3);

    /* Read each sample of one pixel back through a single-sampled blit --
     * glReadPixels on a multisample target is not allowed, so each sample is
     * resolved on its own by attaching the multisample texture to a reader
     * that selects it. GL has no such selector, so the shader below does it:
     * it samples the multisample texture at an explicit sample index. */
    GLuint single = 0, single_fbo = 0;
    glGenTextures(1, &single);
    glBindTexture(GL_TEXTURE_2D, single);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA16F, SIDE, SIDE, 0, GL_RGBA,
                 GL_FLOAT, NULL);
    glGenFramebuffers(1, &single_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, single_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           single, 0);
    GLuint fetch = build(
        "#version 400 core\n"
        "uniform sampler2DMS src;\n"
        "uniform int which;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  colour = texelFetch(src, ivec2(gl_FragCoord.xy), which);\n"
        "}\n");
    int agrees = fetch != 0;
    float shader_pos[SAMPLES][2] = {{0}};
    for (int i = 0; i < SAMPLES && fetch; ++i) {
        glBindFramebuffer(GL_FRAMEBUFFER, single_fbo);
        glViewport(0, 0, SIDE, SIDE);
        glDisable(GL_SAMPLE_SHADING);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(fetch);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms);
        glUniform1i(glGetUniformLocation(fetch, "src"), 0);
        glUniform1i(glGetUniformLocation(fetch, "which"), i);
        glBufferData(GL_ARRAY_BUFFER, sizeof(full), full, GL_STREAM_DRAW);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        float px[4] = {0};
        glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_FLOAT, px);
        shader_pos[i][0] = px[0];
        shader_pos[i][1] = px[1];
        if (fabsf(px[2] - 0.75f) > 1e-2f) {
            fprintf(stderr, "INFO sample %d was never written (marker %.3f)\n",
                    i, px[2]);
            agrees = 0;
        }
        if (fabsf(px[0] - queried[i][0]) > 1e-3f ||
            fabsf(px[1] - queried[i][1]) > 1e-3f)
            agrees = 0;
    }
    error = glGetError();
    fprintf(stderr, "INFO shader (%.3f,%.3f) (%.3f,%.3f) (%.3f,%.3f) "
            "(%.3f,%.3f) err=0x%04x\n",
            shader_pos[0][0], shader_pos[0][1], shader_pos[1][0],
            shader_pos[1][1], shader_pos[2][0], shader_pos[2][1],
            shader_pos[3][0], shader_pos[3][1], (unsigned)error);
    result("agrees", agrees && error == GL_NO_ERROR);

    /* A half-covering triangle: the samples of the pixels along its edge must
     * not all agree, or the positions are four numbers that match each other
     * and describe nothing. The colour is the sample id, so a covered sample
     * reads its own index and an uncovered one keeps the clear. */
    GLuint tag = build(
        "#version 400 core\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(float(gl_SampleID + 1), 0.0, 0.0, 1.0); }\n");
    int mixed = 0;
    if (tag && fetch) {
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glViewport(0, 0, SIDE, SIDE);
        glEnable(GL_SAMPLE_SHADING);
        glMinSampleShading(1.0f);
        glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(tag);
        /* A vertical edge through the middle of pixel column 2, which is the
         * pixel read back below. Its four samples sit at x offsets 0.125,
         * 0.375, 0.625 and 0.875 within the pixel, so an edge at 0.5 leaves
         * two on each side however the table is ordered -- and a driver whose
         * samples are all at the pixel centre puts all four on one side. */
        const float half[8] = {
            -1.0f, -1.0f,   0.25f, -1.0f,
            -1.0f,  1.0f,   0.25f,  1.0f,
        };
        glBufferData(GL_ARRAY_BUFFER, sizeof(half), half, GL_STREAM_DRAW);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        int covered[SAMPLES] = {0};
        for (int i = 0; i < SAMPLES; ++i) {
            glBindFramebuffer(GL_FRAMEBUFFER, single_fbo);
            glDisable(GL_SAMPLE_SHADING);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(fetch);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms);
            glUniform1i(glGetUniformLocation(fetch, "src"), 0);
            glUniform1i(glGetUniformLocation(fetch, "which"), i);
            glBufferData(GL_ARRAY_BUFFER, sizeof(full), full, GL_STREAM_DRAW);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            float px[4] = {0};
            /* The pixel the diagonal edge runs through. */
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_FLOAT, px);
            covered[i] = px[0] > 0.5f;
        }
        fprintf(stderr, "INFO covers %d %d %d %d\n", covered[0], covered[1],
                covered[2], covered[3]);
        for (int i = 1; i < SAMPLES; ++i)
            if (covered[i] != covered[0]) mixed = 1;
    }
    result("covers", mixed);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
