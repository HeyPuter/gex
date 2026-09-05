/* A vertex shader that *writes* a storage buffer.
 *
 * WGSL forbids `var<storage, read_write>` in the vertex stage outright, so
 * Tint refuses the module and the program fails to link. Reading one is fine
 * and already works -- see gl44-vertex-ssbo-read-probe, which also records why
 * inferring the read-only qualifier is redundant. Writing is the real wall.
 *
 * It is not an exotic shape. A vertex shader that records what it processed is
 * how GPU-driven rendering builds draw lists, how occlusion feedback is
 * gathered, and how a compaction pass reports its counts. About 79 cases of
 * KHR-GL45.shader_storage_buffer_object and its es_31_compatibility twin are
 * this and nothing else.
 *
 * This driver already runs a vertex shader twice for exactly this reason:
 * transform feedback dispatches the shader as compute "because WGSL will not
 * let the vertex stage write a storage buffer", then rasterizes normally. The
 * same shape serves here -- dispatch for the side effects, rasterize with a
 * variant whose stores are gone.
 *
 *   `links`    the program links at all. On a driver that refuses the module
 *              this fails first and the rest cannot run, which is why it is
 *              separate: a link failure and a wrong value look nothing alike
 *              and should not share a case.
 *   `draws`    the quad still reaches the framebuffer. A store has no result,
 *              so removing it must not change what is drawn.
 *   `wrote`    the buffer holds what the vertex shader put there, indexed by
 *              gl_VertexID so that a shared vertex written twice is
 *              idempotent -- the compute pass runs once per *assembled*
 *              vertex, and GL does not promise one invocation per unique
 *              vertex either.
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

#define SIDE 32

static const char *vs_write =
    "#version 430 core\n"
    "layout(std430, binding = 0) buffer Seen { uint at[4]; } seen;\n"
    "const vec2 corner[4] = vec2[4](vec2(-1.0,-1.0), vec2(0.0,-1.0),\n"
    "                               vec2(-1.0, 1.0), vec2(0.0, 1.0));\n"
    "out vec3 tint;\n"
    "void main() {\n"
    "  seen.at[gl_VertexID] = uint(gl_VertexID) + 100u;\n"
    "  gl_Position = vec4(corner[gl_VertexID], 0.0, 1.0);\n"
    "  tint = vec3(0.0, 1.0, 0.0);\n"
    "}\n";
static const char *fs_plain =
    "#version 430 core\n"
    "in vec3 tint;\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(tint, 1.0); }\n";

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
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
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

    unsigned zero[4] = {0, 0, 0, 0};
    GLuint seen = 0;
    glGenBuffers(1, &seen);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, seen);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zero), zero, GL_STATIC_DRAW);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, seen);

    GLuint prog = glCreateProgram();
    struct { GLenum stage; const char *src; } parts[] = {
        {GL_VERTEX_SHADER, vs_write}, {GL_FRAGMENT_SHADER, fs_plain},
    };
    int compiled = 1;
    for (unsigned i = 0; i < 2; ++i) {
        GLuint sh = glCreateShader(parts[i].stage);
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO compile: %s\n", log);
            compiled = 0;
        }
        glAttachShader(prog, sh);
        glDeleteShader(sh);
    }
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
    }
    result("links", compiled && linked);

    unsigned char left[4] = {0}, right[4] = {0};
    unsigned got[4] = {0, 0, 0, 0};
    if (linked) {
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(prog);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);
        glReadPixels(SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, left);
        glReadPixels(3 * SIDE / 4, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     right);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, seen);
        const unsigned *mapped = (const unsigned *)glMapBufferRange(
            GL_SHADER_STORAGE_BUFFER, 0, sizeof(got), GL_MAP_READ_BIT);
        if (mapped) memcpy(got, mapped, sizeof(got));
        glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    }
    const GLenum error = glGetError();
    fprintf(stderr, "INFO draws left=%u,%u,%u right=%u,%u,%u err=0x%04x\n",
            left[0], left[1], left[2], right[0], right[1], right[2],
            (unsigned)error);
    result("draws", linked && error == GL_NO_ERROR && left[1] > 128 &&
                    right[1] < 64);

    fprintf(stderr, "INFO wrote %u %u %u %u (want 100 101 102 103)\n",
            got[0], got[1], got[2], got[3]);
    result("wrote", linked && got[0] == 100 && got[1] == 101 &&
                    got[2] == 102 && got[3] == 103);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
