/* ARB_framebuffer_no_attachments -- one of GL 4.3's five remaining items.
 *
 * A framebuffer with *nothing attached to it*, made complete by
 * glFramebufferParameteri(GL_FRAMEBUFFER_DEFAULT_WIDTH / HEIGHT / LAYERS /
 * SAMPLES). Rendering into it runs the fragment shader over the covered area
 * and throws the colour away, which sounds pointless until you notice what a
 * modern renderer uses it for: the fragment shader's *side effects*. Voxel
 * cone tracing rasterises geometry into a 3D image this way; GPU-driven
 * culling counts visible fragments into a storage buffer; order-independent
 * transparency builds its per-pixel linked lists. All of them want the
 * rasterizer without a colour target.
 *
 * WebGPU has no such thing -- a render pass must have at least one attachment
 * -- so the answer is a scratch attachment at the default size whose store op
 * is discard. That is why the size cases below matter more than they look:
 * a driver that allocates the scratch target at the wrong size, or ignores
 * the viewport, produces the right *kind* of result with the wrong number of
 * fragments, and no pixel exists to compare.
 *
 * So every case counts fragments into a storage buffer and checks the count
 * exactly:
 *
 *   `complete`       the framebuffer is GL_FRAMEBUFFER_COMPLETE with no
 *                    attachments at all
 *   `params`         the default width and height read back as they were set
 *   `full`           a full-viewport quad over a 64x48 default size runs the
 *                    fragment shader 64*48 times
 *   `viewport`       the same framebuffer with a 16x8 viewport runs it 16*8
 *                    times, which is what catches a scratch target sized from
 *                    the viewport rather than from the default
 *   `scissor`        and with a scissor inside that, 4*4 times
 *
 * The counter is an atomic add into an SSBO, so the count is exact rather
 * than a colour to compare -- and a driver with no rasterization at all
 * reports zero, which is unmistakable.
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

#define DEFAULT_W 64
#define DEFAULT_H 48

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

static GLuint counting_program(void)
{
    static const char *vs_src =
        "#version 330 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 1.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 1.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0);\n"
        "}\n";
    /* No colour output at all: there is nowhere to put one, and declaring one
     * is what a driver emulating this with a scratch attachment must not
     * require.
     *
     * Each fragment marks *its own* slot rather than incrementing a counter.
     * An atomic add would be the obvious way to count and it tests two things
     * at once: gex_wgpu refuses storage atomics today, so the probe reported
     * zero fragments for a framebuffer that was working. Marking a slot needs
     * no atomic -- one fragment per slot, so there is no race -- and it is a
     * stronger check, because the CPU then knows *which* pixels were covered
     * rather than only how many. */
    static const char *fs_src =
        "#version 330 core\n"
        "#extension GL_ARB_shader_storage_buffer_object : require\n"
        "layout(std430, binding = 0) buffer Covered { uint hit[]; };\n"
        "void main() {\n"
        "  ivec2 p = ivec2(gl_FragCoord.xy);\n"
        "  hit[p.y * 64 + p.x] = 1u;\n"
        "}\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLint ok = 0;
    glGetShaderiv(vs, GL_COMPILE_STATUS, &ok);
    if (!ok) return 0;
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
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
    glDeleteShader(vs);
    glDeleteShader(fs);
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

static GLuint counter_bo;
#define COVERAGE_SLOTS (DEFAULT_W * DEFAULT_H)

/* Draw the quad and return how many distinct pixels the fragment shader ran
 * on. `box` receives the covered rectangle, so a wrong *placement* with the
 * right area is caught too. */
static unsigned count_fragments(GLuint prog, int box[4])
{
    static uint32_t zeros[COVERAGE_SLOTS];
    memset(zeros, 0, sizeof(zeros));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, counter_bo);
    glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(zeros), zeros);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, counter_bo);
    glUseProgram(prog);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
    unsigned got = 0;
    int minx = 1 << 20, miny = 1 << 20, maxx = -1, maxy = -1;
    const uint32_t *read = (const uint32_t *)glMapBufferRange(
        GL_SHADER_STORAGE_BUFFER, 0, sizeof(zeros), GL_MAP_READ_BIT);
    if (read) {
        for (unsigned i = 0; i < COVERAGE_SLOTS; ++i) {
            if (!read[i])
                continue;
            ++got;
            const int x = (int)(i % DEFAULT_W), y = (int)(i / DEFAULT_W);
            if (x < minx) minx = x;
            if (y < miny) miny = y;
            if (x > maxx) maxx = x;
            if (y > maxy) maxy = y;
        }
        glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    } else {
        return 0xffffffffu;
    }
    if (box) {
        box[0] = maxx < 0 ? -1 : minx;
        box[1] = maxy < 0 ? -1 : miny;
        box[2] = maxx;
        box[3] = maxy;
    }
    return got;
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

    const int have = have_extension("GL_ARB_framebuffer_no_attachments");
    GLint max_fb_w = 0, max_fb_h = 0, max_layers = 0;
    glGetIntegerv(GL_MAX_FRAMEBUFFER_WIDTH, &max_fb_w);
    glGetIntegerv(GL_MAX_FRAMEBUFFER_HEIGHT, &max_fb_h);
    glGetIntegerv(GL_MAX_FRAMEBUFFER_LAYERS, &max_layers);
    while (glGetError() != GL_NO_ERROR) {}
    fprintf(stderr, "INFO version=%s no_attachments=%d max_fb=%dx%d "
            "max_layers=%d\n", glGetString(GL_VERSION), have, max_fb_w,
            max_fb_h, max_layers);

    GLuint vao = 0, fbo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &counter_bo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, counter_bo);
    {
        static uint32_t zeros[COVERAGE_SLOTS];
        memset(zeros, 0, sizeof(zeros));
        glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zeros), zeros,
                     GL_DYNAMIC_COPY);
    }
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    GLenum status = 0;
    GLint got_w = 0, got_h = 0;
    if (have) {
        glFramebufferParameteri(GL_FRAMEBUFFER, GL_FRAMEBUFFER_DEFAULT_WIDTH,
                                DEFAULT_W);
        glFramebufferParameteri(GL_FRAMEBUFFER, GL_FRAMEBUFFER_DEFAULT_HEIGHT,
                                DEFAULT_H);
        glFramebufferParameteri(GL_FRAMEBUFFER, GL_FRAMEBUFFER_DEFAULT_LAYERS,
                                0);
        glFramebufferParameteri(GL_FRAMEBUFFER, GL_FRAMEBUFFER_DEFAULT_SAMPLES,
                                0);
        glGetFramebufferParameteriv(GL_FRAMEBUFFER,
                                    GL_FRAMEBUFFER_DEFAULT_WIDTH, &got_w);
        glGetFramebufferParameteriv(GL_FRAMEBUFFER,
                                    GL_FRAMEBUFFER_DEFAULT_HEIGHT, &got_h);
        glDrawBuffer(GL_NONE);
        glReadBuffer(GL_NONE);
        status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    }
    const GLenum setup_error = glGetError();
    fprintf(stderr, "INFO complete status=0x%04x err=0x%04x\n",
            (unsigned)status, (unsigned)setup_error);
    result("complete", have && status == GL_FRAMEBUFFER_COMPLETE &&
                       setup_error == GL_NO_ERROR);
    fprintf(stderr, "INFO params got=%dx%d want=%dx%d\n", got_w, got_h,
            DEFAULT_W, DEFAULT_H);
    result("params", have && got_w == DEFAULT_W && got_h == DEFAULT_H);

    GLuint prog = have ? counting_program() : 0;
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);

    /* Case 3: the whole default size. */
    {
        unsigned got = 0;
        int box[4] = {-1, -1, -1, -1};
        if (prog) {
            glDisable(GL_SCISSOR_TEST);
            glViewport(0, 0, DEFAULT_W, DEFAULT_H);
            got = count_fragments(prog, box);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO full got=%u want=%u box=%d,%d..%d,%d "
                "err=0x%04x\n", got, DEFAULT_W * DEFAULT_H, box[0], box[1],
                box[2], box[3], (unsigned)error);
        result("full", prog && error == GL_NO_ERROR &&
                       got == DEFAULT_W * DEFAULT_H &&
                       box[0] == 0 && box[1] == 0 &&
                       box[2] == DEFAULT_W - 1 && box[3] == DEFAULT_H - 1);
    }

    /* Case 4: a smaller viewport on the same framebuffer. */
    {
        unsigned got = 0;
        int box[4] = {-1, -1, -1, -1};
        if (prog) {
            glDisable(GL_SCISSOR_TEST);
            glViewport(0, 0, 16, 8);
            got = count_fragments(prog, box);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO viewport got=%u want=%u box=%d,%d..%d,%d "
                "err=0x%04x\n", got, 16u * 8u, box[0], box[1], box[2], box[3],
                (unsigned)error);
        result("viewport", prog && error == GL_NO_ERROR && got == 16u * 8u &&
                           box[0] == 0 && box[1] == 0 && box[2] == 15 &&
                           box[3] == 7);
    }

    /* Case 5: and a scissor inside that. */
    {
        unsigned got = 0;
        int box[4] = {-1, -1, -1, -1};
        if (prog) {
            glViewport(0, 0, 16, 8);
            glEnable(GL_SCISSOR_TEST);
            glScissor(2, 2, 4, 4);
            got = count_fragments(prog, box);
            glDisable(GL_SCISSOR_TEST);
        }
        const GLenum error = glGetError();
        fprintf(stderr, "INFO scissor got=%u want=%u box=%d,%d..%d,%d "
                "err=0x%04x\n", got, 4u * 4u, box[0], box[1], box[2], box[3],
                (unsigned)error);
        result("scissor", prog && error == GL_NO_ERROR && got == 4u * 4u &&
                          box[0] == 2 && box[1] == 2 && box[2] == 5 &&
                          box[3] == 5);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
